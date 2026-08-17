use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use editor::{Editor, EditorEvent};
use gpui::{
    Anchor, AnyElement, App, Bounds, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, RenderImage, ScrollHandle, SharedString, Subscription, Task, Window, anchored, canvas,
    deferred, div, img, point, px,
};
use pdfium_render::prelude::PdfPageIndex;
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use ui::{ContextMenu, Tooltip, WithScrollbar, prelude::*};
use workspace::{Item, ToolbarItemLocation, item::ItemEvent};

use crate::{
    PdfBlackScreen, PdfContents, PdfCopy, PdfFind, PdfFindNext, PdfFindPrevious, PdfFirstPage,
    PdfFitPage, PdfFitWidth, PdfFullScreen, PdfGoToPage, PdfLastPage, PdfNextPage, PdfNightMode,
    PdfOnePage, PdfPresent, PdfPreviousPage, PdfPrint, PdfProperties, PdfRotate, PdfRotateBack,
    PdfSaveACopy, PdfSelectPage, PdfTenPagesBack, PdfTenPagesOn, PdfThumbnails, PdfTwoAcross,
    PdfWhiteScreen, PdfZoomIn, PdfZoomOut, PdfZoomReset, pdf_engine, pdf_item::PdfItem,
};

use project::Project;
use workspace::Pane;

/// Width a page is rendered at before the reader has zoomed. Rendering follows
/// this rather than the window, so a page keeps its size while the pane is
/// resized and only the engine's own work decides how sharp it is.
const BASE_PAGE_WIDTH: f32 = 900.;

/// What one press of zoom does, and how far it may go. A page below the lower
/// bound is unreadable, and one above the upper costs more to render than any
/// reader wants to wait for.
const ZOOM_STEP: f32 = 0.2;
const ZOOM_MIN: f32 = 0.1;
/// The sizes offered by name, the way a reader asks for them.
const ZOOM_CHOICES: [f32; 8] = [0.5, 0.75, 1., 1.25, 1.5, 2., 3., 4.];

/// How often the file itself is looked at, to notice a document rewritten under
/// the reader. Often enough to feel immediate, seldom enough to cost nothing.
const HOW_OFTEN_THE_FILE_IS_LOOKED_AT: Duration = Duration::from_secs(2);
const ZOOM_MAX: f32 = 4.0;

/// How many pages are rendered eagerly. The rest follow as they are scrolled to.
const PAGES_RENDERED_AT_ONCE: usize = 3;

/// Room left around a page when it is scaled to the window, so a fitted page is
/// not pressed against the edge.
const FITTING_MARGIN: f32 = 32.;

/// The most pixels a page is ever drawn across. A dense screen at a deep zoom
/// would otherwise ask for a picture of tens of megabytes a page: an A4 page four
/// times over on a screen with two pixels to a laid-out one is 7200 across and
/// nearly 300MB. Past this the picture is scaled up instead, which is softer than
/// drawing it again and far cheaper than holding it.
const MOST_PIXELS_ACROSS: f32 = 3000.;

/// How far from what the reader is looking at a drawn page is kept. Pages are
/// held as pictures, and a long document read to the end would otherwise hold
/// every one of them at once.
const PAGES_KEPT_EITHER_SIDE: usize = 4;

/// The width a page's picture in the side list is drawn at.
const THUMBNAIL_WIDTH: u32 = 120;

/// How far the zoom has to move before the pages are drawn again. Fitting works
/// out a zoom from the window's own size, which changes by fractions of a pixel
/// as a pane is dragged; without this every one of those would throw away every
/// page and draw it again.
const ZOOM_WORTH_REDRAWING: f32 = 0.005;

/// The widths at which the strip of controls starts leaving things out. Below
/// the first, what is reached for least goes into the menu; below the second,
/// only moving about and zooming are left on the strip.
const ROOM_FOR_EVERYTHING: f32 = 820.;
const ROOM_FOR_SEARCHING: f32 = 660.;
const ROOM_FOR_SIDE_LISTS: f32 = 420.;
const ROOM_FOR_PAGE_NUMBERS: f32 = 300.;
const ROOM_FOR_A_ZOOM_LABEL: f32 = 200.;

/// How pages are scaled to the window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fit {
    /// Whatever the reader zoomed to.
    Free,
    /// One page fills the width of the window.
    Width,
    /// A whole page is in view.
    Page,
}

/// One place a search found what it was looking for: which page, and where on it
/// in the points the page is laid out in.
#[derive(Clone, Copy, Debug)]
struct Found {
    page: usize,
    bottom: f32,
    left: f32,
    top: f32,
    right: f32,
}

/// A blank screen held up during a slideshow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Blank {
    Black,
    White,
}

/// What the side list shows, if anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sidebar {
    Hidden,
    Thumbnails,
    Outline,
}

/// A drag smaller than this fraction of the page in either direction is a click
/// that wandered rather than a selection.
const SMALLEST_SELECTION: f32 = 0.004;

/// What the reader has dragged over a page, kept as fractions of that page
/// rather than positions in the window: a page moves when the document is
/// scrolled or zoomed, and a selection has to stay on the words it was drawn
/// around.
#[derive(Clone, Debug)]
struct Selection {
    page: usize,
    from: Point<f32>,
    to: Point<f32>,
    /// Whether the pointer is still down. A finished selection stays on screen
    /// so it can be copied.
    dragging: bool,
}

impl Selection {
    /// Left, top, right, bottom, in fractions of the page.
    fn corners(&self) -> (f32, f32, f32, f32) {
        (
            self.from.x.min(self.to.x),
            self.from.y.min(self.to.y),
            self.from.x.max(self.to.x),
            self.from.y.max(self.to.y),
        )
    }

    /// The same rectangle in the points a page is laid out in, with the origin at
    /// its bottom left rather than its top.
    fn is_worth_reading(&self) -> bool {
        let (left, top, right, bottom) = self.corners();
        right - left >= SMALLEST_SELECTION && bottom - top >= SMALLEST_SELECTION
    }
}

/// Where a point on the painted page sits on the page as the document lays it
/// out, both as fractions from the top left. A turned page is painted with its
/// sides swapped, and everything the engine knows about it is in its own
/// unturned frame.
fn as_the_document_lays_it_out(at: Point<f32>, quarter_turns: u8) -> Point<f32> {
    match quarter_turns % 4 {
        1 => point(at.y, 1. - at.x),
        2 => point(1. - at.x, 1. - at.y),
        3 => point(1. - at.y, at.x),
        _ => at,
    }
}

/// The way back: a place on the page as laid out, to where it is painted.
fn as_it_is_painted(at: Point<f32>, quarter_turns: u8) -> Point<f32> {
    match quarter_turns % 4 {
        1 => point(1. - at.y, at.x),
        2 => point(1. - at.x, 1. - at.y),
        3 => point(at.y, 1. - at.x),
        _ => at,
    }
}

/// The character of `characters` nearest `at`, which is in page points. One the
/// point is inside wins; otherwise the nearest by the distance to its box, so a
/// drag through a margin still marks the line beside it.
fn character_nearest(characters: &[pdf_engine::PageChar], at: (f32, f32)) -> Option<usize> {
    let (x, y) = at;
    let mut nearest = None;
    let mut nearest_distance = f32::MAX;
    for (index, character) in characters.iter().enumerate() {
        let inside = x >= character.left
            && x <= character.right
            && y >= character.bottom
            && y <= character.top;
        if inside {
            return Some(index);
        }
        let across = (character.left - x).max(x - character.right).max(0.);
        let down = (character.bottom - y).max(y - character.top).max(0.);
        // Along the line counts for less than across lines: a point to the right
        // of a line's last word belongs to that line, not to the one below it.
        let distance = down * down * 4. + across * across;
        if distance < nearest_distance {
            nearest_distance = distance;
            nearest = Some(index);
        }
    }
    nearest
}

/// The rectangles to mark a run of characters with, in page points: one for each
/// line the run covers, since a selection over three lines is three bands and not
/// one box around them all.
fn bands_over(
    characters: &[pdf_engine::PageChar],
    covered: std::ops::Range<usize>,
) -> Vec<(f32, f32, f32, f32)> {
    let mut bands: Vec<(f32, f32, f32, f32)> = Vec::new();
    for character in characters
        .get(covered)
        .unwrap_or_default()
        .iter()
        .filter(|character| !character.character.is_whitespace())
    {
        let (bottom, left, top, right) = (
            character.bottom,
            character.left,
            character.top,
            character.right,
        );
        match bands.last_mut() {
            // The same line, going by whether the two overlap up and down: a
            // character's box is as tall as its line, and the next line's does
            // not reach into it.
            Some(band) if bottom < band.2 && top > band.0 => {
                band.0 = band.0.min(bottom);
                band.1 = band.1.min(left);
                band.2 = band.2.max(top);
                band.3 = band.3.max(right);
            }
            _ => bands.push((bottom, left, top, right)),
        }
    }
    bands
}

pub struct PdfView {
    path: PathBuf,
    title: SharedString,
    /// Rendered pages by index. A page absent from here has not been rendered at
    /// this zoom yet, and shows its place until it has.
    pages: Vec<Option<Arc<RenderImage>>>,
    /// Each page's own size, in the points it is laid out in.
    page_sizes: Vec<pdf_engine::PageSize>,
    /// Where each page was last painted, so a position in the window can be told
    /// which page it is on and where on it.
    page_bounds: Vec<Rc<Cell<Bounds<Pixels>>>>,
    /// Where the scrolling area itself was last painted, which is what makes a
    /// page "on screen" or not.
    viewport: Rc<Cell<Bounds<Pixels>>>,
    /// How wide the strip of controls was last painted. What it holds follows
    /// from this: a control that would hang past the edge of the window is worse
    /// than one the reader has to open a menu for.
    room_for_controls: Rc<Cell<Pixels>>,
    zoom: f32,
    focus: FocusHandle,
    scroll: ScrollHandle,
    sidebar_scroll: ScrollHandle,
    opening: Option<Task<()>>,
    /// Which reading of the document everything held belongs to. A document read
    /// again -- rewritten under the reader, or opened with a password -- starts a
    /// new one, and answers about the old reading are dropped rather than written
    /// over the new document's pages.
    reading: u64,
    /// Each page's links, so a click on one can be followed.
    links: HashMap<usize, Rc<Vec<pdf_engine::PageLink>>>,
    links_asked_for: HashSet<usize>,
    reading_links: Vec<Task<()>>,
    /// Set when the document will not open without a password, with the field the
    /// reader types it into.
    asks_for_a_password: bool,
    password_field: Entity<Editor>,
    /// When the file was last changed, so a document rewritten under the reader is
    /// read again.
    last_changed: Option<std::time::SystemTime>,
    _watching_the_file: Task<()>,
    /// Whether pages are shown two across, as a book is read.
    two_across: bool,
    /// Where the pointer was when panning started, if it is panning.
    panning: Option<Point<Pixels>>,
    /// Each page's characters and where they sit, once read. Selecting text means
    /// asking which character the pointer is over on every move, so this is held
    /// rather than asked of the engine each time.
    chars: HashMap<usize, Rc<Vec<pdf_engine::PageChar>>>,
    chars_asked_for: HashSet<usize>,
    reading_chars: Vec<Task<()>>,
    /// Pages already asked for, so a page is not rendered twice while the first
    /// answer is still on its way.
    asked_for: HashSet<usize>,
    /// Pages that failed to draw once and were given another go. A second
    /// failure is left where it is: a page asked for again the moment it fails
    /// is not a retry but a spin, and a document the engine cannot read at all
    /// would spin on every page of it.
    given_another_go: HashSet<usize>,
    rendering: Vec<Task<()>>,
    trouble: Option<SharedString>,
    selection: Option<Selection>,
    /// The text under the finished selection, once the engine has read it.
    selected_text: Option<SharedString>,
    reading_text: Option<Task<()>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// Quarter turns clockwise applied to every page.
    quarter_turns: u8,
    /// Whether the pages are shown with their colours turned over.
    at_night: bool,
    /// Whether the document is being shown as a slideshow, and what the reader
    /// was looking at before it started, to be put back afterwards.
    presenting: bool,
    before_presenting: Option<(bool, Fit, Sidebar)>,
    /// A blank screen over the slideshow, for taking the room's attention.
    blanked: Option<Blank>,
    /// What was searched for, and every place it was found, in document order.
    found: Vec<Found>,
    /// Whether the search minds the case of what was typed, and whether it only
    /// counts whole words.
    match_case: bool,
    whole_words: bool,
    searched_for: Option<SharedString>,
    searching: Option<Task<()>>,
    /// The field the reader types what to look for into.
    find_editor: Entity<Editor>,
    searching_now: bool,
    /// Which of the pages the words were found on is being shown.
    at_match: usize,
    /// The field a page number is typed into.
    page_field: Entity<Editor>,
    /// How many screen pixels there are to a laid-out one. Pages are drawn in
    /// screen pixels and painted at their laid-out size, or a page on a dense
    /// screen is drawn at half the size it is shown at and reads as a blur.
    screen_pixels: f32,
    /// How pages are scaled to the window, and the window size that scaling was
    /// worked out for, so it is worked out again only when the window changes.
    fit: Fit,
    fitted_for: Option<(f32, f32)>,
    sidebar: Sidebar,
    /// Whether the reader is shown one page at a time rather than a column of
    /// them, and which page that is. Scrolling cannot say which page is being
    /// read when only one is there, so it is held rather than worked out.
    one_page_at_a_time: bool,
    showing: usize,
    /// A page to scroll to as soon as the column it is in has been laid out.
    bring_into_view: Option<usize>,
    /// Small pictures of the pages, drawn only while the side list is showing.
    thumbnails: Vec<Option<Arc<RenderImage>>>,
    thumbnails_asked_for: HashSet<usize>,
    drawing_thumbnails: Vec<Task<()>>,
    /// The document's own table of contents, read once.
    outline: Vec<Outline>,
    reading_outline: Option<Task<()>>,
    /// What the document says about itself, shown when the reader asks.
    facts: Option<SharedString>,
    reading_facts: Option<Task<()>>,
    saving: Option<Task<()>>,
    _find_subscription: Subscription,
    _page_subscription: Subscription,
}

/// One line of the document's table of contents, as the side list shows it.
#[derive(Clone)]
struct Outline {
    title: SharedString,
    page: usize,
    depth: usize,
}

impl PdfView {
    pub fn new(item: Entity<PdfItem>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = item.read(cx).abs_path.clone();
        Self::open_path(path, window, cx)
    }

    /// Opens a document by its path. What the editor holds about the file beyond
    /// where it is does not reach the engine, which reads the file itself.
    pub fn open_path(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "PDF".to_string())
            .into();

        let find_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Find in document", window, cx);
            editor
        });
        // Searching as the reader types would read the whole document on every
        // keystroke, so the search waits to be asked.
        let find_subscription =
            cx.subscribe(&find_editor, |view, editor, event: &EditorEvent, cx| {
                if !matches!(event, EditorEvent::Blurred) {
                    return;
                }
                let needle: SharedString = editor.read(cx).text(cx).into();
                view.find(needle, cx);
            });

        let page_field = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Page", window, cx);
            editor
        });
        // A number is acted on when the field is left or Enter is pressed, not
        // while it is being typed: half of "12" is page 1, and jumping there
        // would take the reader away mid-keystroke.
        let page_subscription =
            cx.subscribe(&page_field, |view, editor, event: &EditorEvent, cx| {
                if !matches!(event, EditorEvent::Blurred) {
                    return;
                }
                let typed = editor.read(cx).text(cx);
                view.go_to_typed_page(&typed, cx);
            });

        let password_field = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Password", window, cx);
            // What is typed is not shown: a document's password is a secret like
            // any other, and this field sits in the middle of the window.
            editor.set_masked(true, cx);
            editor
        });

        // A document rewritten under the reader -- a report built again, a file
        // synced -- is read again rather than left as it was.
        let watched = path.clone();
        let watching = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(HOW_OFTEN_THE_FILE_IS_LOOKED_AT)
                    .await;
                let changed = smol::fs::metadata(&watched)
                    .await
                    .ok()
                    .and_then(|file| file.modified().ok());
                let carry_on = view
                    .update(cx, |view, cx| match (changed, view.last_changed) {
                        (Some(now), Some(before)) if now != before => {
                            view.last_changed = Some(now);
                            view.read_the_document(cx);
                        }
                        (Some(now), None) => view.last_changed = Some(now),
                        _ => {}
                    })
                    .is_ok();
                if !carry_on {
                    return;
                }
            }
        });

        let mut view = Self {
            path,
            title,
            pages: Vec::new(),
            page_sizes: Vec::new(),
            page_bounds: Vec::new(),
            viewport: Rc::new(Cell::new(Bounds::default())),
            room_for_controls: Rc::new(Cell::new(Pixels::ZERO)),
            zoom: 1.,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            sidebar_scroll: ScrollHandle::new(),
            opening: None,
            reading: 0,
            links: HashMap::default(),
            links_asked_for: HashSet::default(),
            reading_links: Vec::new(),
            asks_for_a_password: false,
            password_field,
            last_changed: None,
            _watching_the_file: watching,
            two_across: false,
            panning: None,
            chars: HashMap::default(),
            chars_asked_for: HashSet::default(),
            reading_chars: Vec::new(),
            asked_for: HashSet::default(),
            given_another_go: HashSet::default(),
            rendering: Vec::new(),
            trouble: None,
            selection: None,
            selected_text: None,
            reading_text: None,
            context_menu: None,
            quarter_turns: 0,
            at_night: false,
            presenting: false,
            before_presenting: None,
            blanked: None,
            found: Vec::new(),
            match_case: false,
            whole_words: false,
            searched_for: None,
            searching: None,
            find_editor,
            searching_now: false,
            at_match: 0,
            page_field,
            screen_pixels: 1.,
            fit: Fit::Free,
            fitted_for: None,
            sidebar: Sidebar::Hidden,
            one_page_at_a_time: false,
            showing: 0,
            bring_into_view: None,
            thumbnails: Vec::new(),
            thumbnails_asked_for: HashSet::default(),
            drawing_thumbnails: Vec::new(),
            outline: Vec::new(),
            reading_outline: None,
            facts: None,
            reading_facts: None,
            saving: None,
            _find_subscription: find_subscription,
            _page_subscription: page_subscription,
        };
        view.read_the_document(cx);
        view
    }

    /// Counts the pages and measures them. Nothing is rendered here: which pages
    /// are worth rendering is decided by what the reader is looking at.
    fn read_the_document(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        // Everything asked for about the document so far belongs to the reading
        // before this one, and its answers are no longer wanted.
        self.reading = self.reading.wrapping_add(1);
        let reading = self.reading;
        self.opening = Some(cx.spawn(async move |view, cx| {
            let read = cx
                .background_spawn(async move {
                    let sizes = pdf_engine::page_sizes(&path)?;
                    anyhow::Ok(sizes)
                })
                .await;

            view.update(cx, |view, cx| {
                if view.reading != reading {
                    return;
                }
                match read {
                    Ok(sizes) => {
                        view.pages = vec![None; sizes.len()];
                        view.page_bounds = (0..sizes.len())
                            .map(|_| Rc::new(Cell::new(Bounds::default())))
                            .collect();
                        view.page_sizes = sizes;
                        view.asked_for.clear();
                        view.given_another_go.clear();
                        view.chars.clear();
                        view.chars_asked_for.clear();
                        view.links.clear();
                        view.links_asked_for.clear();
                        view.asks_for_a_password = false;
                        view.trouble = None;
                        // A fit worked out before the pages had been measured was
                        // worked out from nothing, and the window has not changed
                        // size since, so only this can ask for it again.
                        view.fitted_for = None;
                        // The first pages, so the document is not a column of
                        // empty frames before anything has been scrolled.
                        view.ask_for_pages(0..PAGES_RENDERED_AT_ONCE, cx);
                    }
                    Err(error) => {
                        // A document that will not open without a password is not
                        // a document that failed to open: the reader is asked for
                        // one instead of being shown the engine's complaint.
                        if pdf_engine::needs_a_password(&view.path) {
                            view.asks_for_a_password = true;
                            view.trouble = None;
                        } else {
                            log::error!("the PDF did not open: {error:#}");
                            view.trouble = Some(format!("{error:#}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Renders any of `wanted` that is missing. Each page is its own errand, so a
    /// slow one does not hold up the page beside it, and a page already asked for
    /// is left alone.
    fn ask_for_pages(&mut self, wanted: std::ops::Range<usize>, cx: &mut Context<Self>) {
        let width = self.render_width();
        let last = self.pages.len();
        for index in wanted.start..wanted.end.min(last) {
            if self.pages[index].is_some() || !self.asked_for.insert(index) {
                continue;
            }
            let path = self.path.clone();
            let at_zoom = self.zoom;
            let turns = self.quarter_turns;
            let at_night = self.at_night;
            let at_density = self.screen_pixels;
            let reading = self.reading;
            self.rendering.push(cx.spawn(async move |view, cx| {
                let drawn = cx
                    .background_spawn(async move {
                        pdf_engine::render_page(
                            &path,
                            index as PdfPageIndex,
                            width,
                            turns,
                            at_night,
                        )
                    })
                    .await;
                view.update(cx, |view, cx| {
                    // The zoom or the turn may have moved on while this was being
                    // drawn. A page drawn for a size nobody is looking at any
                    // more would paint blurred, and one drawn the other way up
                    // would land on top of the page drawn since.
                    // Anything that decides what a page should look like may have
                    // moved on while it was being drawn, and a page drawn for a
                    // state nobody is in any more is worse than none: it paints
                    // blurred, or the wrong way up, or over a page let go of.
                    if view.reading != reading
                        || (view.zoom - at_zoom).abs() > f32::EPSILON
                        || view.quarter_turns != turns
                        || view.at_night != at_night
                        || (view.screen_pixels - at_density).abs() > 0.01
                        || !view.asked_for.contains(&index)
                    {
                        return;
                    }
                    match drawn {
                        Ok(page) => {
                            if let Some(slot) = view.pages.get_mut(index) {
                                *slot = Some(Arc::new(as_render_image(page)));
                            }
                        }
                        Err(error) => {
                            log::warn!("page {index} of the PDF did not render: {error:#}");
                            // Forgetting that it was asked for is what lets it be
                            // asked for again, and a page never asked for again
                            // is a frame for as long as the document is open.
                            // Once, though: the zoom or the turn changing is what
                            // offers it any further go.
                            if view.given_another_go.insert(index) {
                                view.asked_for.remove(&index);
                            }
                        }
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        // Finished errands are dropped so the list does not grow with the
        // document; a task that has run holds nothing worth keeping.
        self.rendering.retain(|task| !task.is_ready());
    }

    /// Reads the characters of any of `wanted` not read yet. A page's worth is a
    /// few thousand of them, so it is read off the interface thread and kept.
    fn ask_for_chars(&mut self, wanted: std::ops::Range<usize>, cx: &mut Context<Self>) {
        for index in wanted.start..wanted.end.min(self.page_sizes.len()) {
            if self.chars.contains_key(&index) || !self.chars_asked_for.insert(index) {
                continue;
            }
            let path = self.path.clone();
            let reading = self.reading;
            self.reading_chars.push(cx.spawn(async move |view, cx| {
                let read = cx
                    .background_spawn(async move {
                        pdf_engine::page_chars(&path, index as PdfPageIndex)
                    })
                    .await;
                view.update(cx, |view, cx| {
                    if view.reading != reading {
                        return;
                    }
                    match read {
                        Ok(characters) => {
                            view.chars.insert(index, Rc::new(characters));
                        }
                        Err(error) => {
                            // Left as asked for: putting it back on the list would
                            // have it asked for again on the very next frame, and
                            // a document whose text cannot be read at all would
                            // spin. Turning the pages or opening it again asks.
                            log::warn!("page {index} would not say what is on it: {error:#}");
                        }
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        self.reading_chars.retain(|task| !task.is_ready());
    }

    /// Takes the password the reader typed and opens the document with it.
    fn try_the_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.password_field.read(cx).text(cx);
        if typed.is_empty() {
            return;
        }
        pdf_engine::remember_password(&self.path, &typed);
        self.password_field.update(cx, |field, cx| {
            // Not left lying in a field on screen once it has been used.
            field.clear(window, cx);
        });
        self.asks_for_a_password = false;
        self.read_the_document(cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The field a document's password is typed into, in place of the pages.
    fn render_password_prompt(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(Label::new("This document is locked."))
            .child(
                h_flex()
                    .w(px(280.))
                    .h(px(28.))
                    .px_2()
                    .border_1()
                    .border_color(ui::cyberpunk::border_dim())
                    .on_action(cx.listener(|view, _: &::menu::Confirm, window, cx| {
                        view.try_the_password(window, cx)
                    }))
                    .child(div().flex_1().child(self.password_field.clone())),
            )
            .child(
                Button::new("pdf-password-open", "Open")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|view, _, window, cx| view.try_the_password(window, cx))),
            )
            .into_any_element()
    }

    /// Shows the pages two across, as a book, or back to one.
    fn show_two_across(&mut self, two_across: bool, cx: &mut Context<Self>) {
        if self.two_across == two_across {
            return;
        }
        self.two_across = two_across;
        // Where the pages sit is about to change, and a fit worked out for one
        // page across does not hold for two.
        for cell in &self.page_bounds {
            cell.set(Bounds::default());
        }
        self.fitted_for = None;
        self.selection = None;
        self.selected_text = None;
        cx.notify();
    }

    /// Reads the links of any of `wanted` not read yet, so a click on one can be
    /// followed.
    fn ask_for_links(&mut self, wanted: std::ops::Range<usize>, cx: &mut Context<Self>) {
        for index in wanted.start..wanted.end.min(self.page_sizes.len()) {
            if self.links.contains_key(&index) || !self.links_asked_for.insert(index) {
                continue;
            }
            let path = self.path.clone();
            let reading = self.reading;
            self.reading_links.push(cx.spawn(async move |view, cx| {
                let read = cx
                    .background_spawn(async move {
                        pdf_engine::page_links(&path, index as PdfPageIndex)
                    })
                    .await;
                view.update(cx, |view, cx| {
                    if view.reading != reading {
                        return;
                    }
                    match read {
                        Ok(links) => {
                            view.links.insert(index, Rc::new(links));
                        }
                        // Left as asked for: putting it back would have it asked
                        // for again on the very next frame.
                        Err(error) => {
                            log::warn!("page {index} would not say what it links to: {error:#}")
                        }
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        self.reading_links.retain(|task| !task.is_ready());
    }

    /// The link under a place on the page, if there is one.
    fn link_under(&self, page: usize, at: Point<Pixels>) -> Option<&pdf_engine::PageLink> {
        let bounds = self.page_bounds.get(page)?.get();
        let page_size = self.page_sizes.get(page)?;
        if bounds.size.width <= px(0.) || page_size.width <= 0. || page_size.height <= 0. {
            return None;
        }
        let across = f32::from(at.x - bounds.origin.x) / f32::from(bounds.size.width);
        let down = f32::from(at.y - bounds.origin.y) / f32::from(bounds.size.height);
        let laid_out = as_the_document_lays_it_out(point(across, down), self.quarter_turns);
        let x = laid_out.x * page_size.width;
        let y = (1. - laid_out.y) * page_size.height;
        self.links
            .get(&page)?
            .iter()
            .find(|link| x >= link.left && x <= link.right && y >= link.bottom && y <= link.top)
    }

    /// Follows a link: to another page of this document, or out to whatever opens
    /// addresses on this machine.
    fn follow_the_link_at(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(page) = self.page_under(at) else {
            return false;
        };
        let leads = match self.link_under(page, at).map(|link| &link.leads) {
            Some(pdf_engine::LinkTarget::Page(page)) => Some(pdf_engine::LinkTarget::Page(*page)),
            Some(pdf_engine::LinkTarget::Address(address)) => {
                Some(pdf_engine::LinkTarget::Address(address.clone()))
            }
            None => None,
        };
        match leads {
            Some(pdf_engine::LinkTarget::Page(page)) => {
                self.show_page(page, cx);
                true
            }
            Some(pdf_engine::LinkTarget::Address(address)) => {
                cx.open_url(&address);
                true
            }
            None => false,
        }
    }

    /// Which pages are on screen, going by where they were last painted, with one
    /// page of margin either side so scrolling meets a drawn page rather than a
    /// frame.
    fn pages_worth_rendering(&self) -> std::ops::Range<usize> {
        if self.one_page_at_a_time {
            let showing = self.showing.min(self.pages.len().saturating_sub(1));
            return match self.pages.is_empty() {
                true => 0..0,
                false => showing..showing + 1,
            };
        }
        let viewport = self.viewport.get();
        if viewport.size.height <= px(0.) || self.page_bounds.is_empty() {
            return 0..PAGES_RENDERED_AT_ONCE.min(self.pages.len());
        }
        let top = viewport.origin.y;
        let bottom = viewport.origin.y + viewport.size.height;
        let mut first = None;
        let mut last = 0usize;
        for (index, cell) in self.page_bounds.iter().enumerate() {
            let bounds = cell.get();
            if bounds.size.height <= px(0.) {
                continue;
            }
            let page_top = bounds.origin.y;
            let page_bottom = bounds.origin.y + bounds.size.height;
            if page_bottom >= top && page_top <= bottom {
                first.get_or_insert(index);
                last = index;
            }
        }
        let first = first.unwrap_or(0);
        first.saturating_sub(1)..(last + 2).min(self.pages.len())
    }

    /// The width pages are drawn at: the base width scaled by the reader's zoom
    /// and again by however many screen pixels there are to a laid-out one.
    fn render_width(&self) -> u32 {
        (BASE_PAGE_WIDTH * self.zoom * self.screen_pixels)
            .min(MOST_PIXELS_ACROSS)
            .round()
            .max(1.) as u32
    }

    /// Lets go of the pages the reader is nowhere near. A page is a picture the
    /// size of the screen it is shown on, and a document read from end to end
    /// would hold every one of them.
    fn let_go_of_distant_pages(&mut self, wanted: &std::ops::Range<usize>) {
        let keep = wanted.start.saturating_sub(PAGES_KEPT_EITHER_SIDE)
            ..(wanted.end + PAGES_KEPT_EITHER_SIDE).min(self.pages.len());
        for index in 0..self.pages.len() {
            if keep.contains(&index) || self.pages[index].is_none() {
                continue;
            }
            self.pages[index] = None;
            // Forgotten as drawn, so coming back to it draws it again rather than
            // leaving a frame where the page was.
            self.asked_for.remove(&index);
            self.given_another_go.remove(&index);
        }
    }

    /// Takes note of how dense the screen is. A window dragged to another screen
    /// changes this, and every page held was drawn for the old one.
    fn drawn_for_this_screen(&mut self, window: &Window, cx: &mut Context<Self>) {
        let now = window.scale_factor().max(1.);
        if (self.screen_pixels - now).abs() < 0.01 {
            return;
        }
        self.screen_pixels = now;
        self.pages = vec![None; self.pages.len()];
        self.asked_for.clear();
        self.given_another_go.clear();
        self.thumbnails = vec![None; self.thumbnails.len()];
        self.thumbnails_asked_for.clear();
        let wanted = self.pages_worth_rendering();
        self.ask_for_pages(wanted, cx);
        cx.notify();
    }

    fn zoom_by(&mut self, step: f32, cx: &mut Context<Self>) {
        // Zooming by hand is an answer to how big the reader wants the page, so
        // it leaves any scaling to the window behind.
        self.fit = Fit::Free;
        self.set_zoom(self.zoom + step, cx);
    }

    fn set_zoom(&mut self, wanted: f32, cx: &mut Context<Self>) {
        let wanted = wanted.clamp(ZOOM_MIN, ZOOM_MAX);
        if (wanted - self.zoom).abs() < ZOOM_WORTH_REDRAWING {
            return;
        }
        self.zoom = wanted;
        // Every page held is at the old width, so they go and are drawn again.
        self.pages = vec![None; self.pages.len()];
        self.asked_for.clear();
        self.given_another_go.clear();
        self.selection = None;
        self.selected_text = None;
        let wanted = self.pages_worth_rendering();
        self.ask_for_pages(wanted, cx);
        cx.notify();
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// The zoom at which a page fills the window the chosen way. None while the
    /// window has not been painted yet, or for a document whose pages have no
    /// size worth scaling by.
    fn zoom_for(&self, fit: Fit) -> Option<f32> {
        let viewport = self.viewport.get();
        let room_across = f32::from(viewport.size.width) - FITTING_MARGIN;
        let room_down = f32::from(viewport.size.height) - FITTING_MARGIN;
        if room_across <= 0. || room_down <= 0. {
            return None;
        }
        let page = self.page_sizes.get(self.page_in_view())?;
        if page.width <= 0. || page.height <= 0. {
            return None;
        }
        // A turned page is as wide as it was tall, and what is being fitted is
        // the page as it is drawn, not as it is stored.
        let (across, down) = match self.quarter_turns % 2 {
            1 => (page.height, page.width),
            _ => (page.width, page.height),
        };
        let by_width = room_across / BASE_PAGE_WIDTH;
        let by_height = room_down / (BASE_PAGE_WIDTH * (down / across));
        let wanted = match fit {
            Fit::Free => return None,
            Fit::Width => by_width,
            Fit::Page => by_width.min(by_height),
        };
        Some(wanted.clamp(ZOOM_MIN, ZOOM_MAX))
    }

    fn fit_to(&mut self, fit: Fit, cx: &mut Context<Self>) {
        self.fit = fit;
        self.fitted_for = None;
        if let Some(zoom) = self.zoom_for(fit) {
            self.set_zoom(zoom, cx);
        }
        cx.notify();
    }

    /// Scales the pages again when the window has changed size under a fit. Only
    /// a real change counts, or the pages would be thrown away and drawn again
    /// on every frame.
    fn keep_the_fit(&mut self, cx: &mut Context<Self>) {
        if self.fit == Fit::Free {
            return;
        }
        let viewport = self.viewport.get();
        let now = (
            f32::from(viewport.size.width),
            f32::from(viewport.size.height),
        );
        if self.fitted_for == Some(now) {
            return;
        }
        self.fitted_for = Some(now);
        if let Some(zoom) = self.zoom_for(self.fit) {
            self.set_zoom(zoom, cx);
        }
    }

    /// Which page a position in the window is over, if any.
    fn page_under(&self, position: Point<Pixels>) -> Option<usize> {
        self.page_bounds
            .iter()
            .position(|bounds| bounds.get().contains(&position))
    }

    fn start_selecting(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_menu(cx);
        // The keyboard has to arrive with the pointer, or the copy that follows
        // this selection is sent to whatever had focus before.
        window.focus(&self.focus, cx);
        let Some(page) = self.page_under(event.position) else {
            self.clear_selection(cx);
            return;
        };
        let Some(on_page) = self.position_on_page(page, event.position) else {
            return;
        };
        self.selected_text = None;
        self.selection = Some(Selection {
            page,
            from: on_page,
            to: on_page,
            dragging: true,
        });
        cx.notify();
    }

    fn keep_selecting(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        if !selection.dragging {
            return;
        }
        let page = selection.page;
        let Some(on_page) = self.position_on_page(page, event.position) else {
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            selection.to = on_page;
        }
        cx.notify();
    }

    fn finish_selecting(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        // A press and release in the same place is a click, and a click on a link
        // follows it. Only then: a drag that happens to end over a link is the
        // reader selecting text, not asking to go somewhere.
        let a_click = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.dragging && !selection.is_worth_reading());
        if a_click && self.follow_the_link_at(event.position, cx) {
            self.selection = None;
            self.selected_text = None;
            cx.notify();
            return;
        }
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        // Letting go reaches here twice, once from the page and once from the
        // window, and reading the text under the selection is worth doing once.
        if !selection.dragging {
            return;
        }
        selection.dragging = false;
        if !selection.is_worth_reading() {
            self.selection = None;
            cx.notify();
            return;
        }
        self.read_the_selected_text(cx);
        cx.notify();
    }

    /// The page most of the window is looking at.
    fn page_in_view(&self) -> usize {
        if self.one_page_at_a_time {
            return self.showing.min(self.pages.len().saturating_sub(1));
        }
        let viewport = self.viewport.get();
        let middle = viewport.origin.y + viewport.size.height / 2.;
        self.page_bounds
            .iter()
            .position(|cell| {
                let bounds = cell.get();
                bounds.size.height > px(0.)
                    && bounds.origin.y <= middle
                    && middle <= bounds.origin.y + bounds.size.height
            })
            .unwrap_or(0)
    }

    /// Brings a page to the top of the window.
    fn show_page(&mut self, page: usize, cx: &mut Context<Self>) {
        if self.one_page_at_a_time {
            self.showing = page.min(self.pages.len().saturating_sub(1));
            self.scroll.set_offset(point(px(0.), px(0.)));
            let wanted = self.pages_worth_rendering();
            self.ask_for_pages(wanted, cx);
            cx.notify();
            return;
        }
        let Some(bounds) = self.page_bounds.get(page).map(|cell| cell.get()) else {
            return;
        };
        let viewport = self.viewport.get();
        let already = self.scroll.offset();
        // The page's place is measured against the window it is painted in, so
        // the distance between the two is what the scroll has to make up.
        let by = bounds.origin.y - viewport.origin.y;
        self.scroll.set_offset(point(already.x, already.y - by));
        cx.notify();
    }

    fn step_page(&mut self, by: isize, cx: &mut Context<Self>) {
        let at = self.page_in_view() as isize;
        let last = self.pages.len().saturating_sub(1) as isize;
        let wanted = (at + by).clamp(0, last.max(0)) as usize;
        self.show_page(wanted, cx);
    }

    fn go_to_typed_page(&mut self, typed: &str, cx: &mut Context<Self>) {
        let typed = typed.trim();
        if typed.is_empty() {
            return;
        }
        // Pages are counted from one where the reader can see them, and from
        // zero everywhere in here.
        let Ok(asked_for) = typed.parse::<usize>() else {
            return;
        };
        if asked_for == 0 || asked_for > self.pages.len() {
            return;
        }
        self.show_page(asked_for - 1, cx);
    }

    /// Steps through the pages the words were found on.
    fn step_match(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.found.is_empty() {
            return;
        }
        let last = self.found.len() as isize;
        let wanted = (self.at_match as isize + by).rem_euclid(last) as usize;
        self.at_match = wanted;
        if let Some(found) = self.found.get(wanted).copied() {
            self.show_page(found.page, cx);
        }
    }

    /// Selects the whole of the page in view, which is what there is to select
    /// when nothing was dragged over.
    fn select_the_page(&mut self, cx: &mut Context<Self>) {
        let page = self.page_in_view();
        if self.page_sizes.get(page).is_none() {
            return;
        }
        self.selection = Some(Selection {
            page,
            from: point(0., 0.),
            to: point(1., 1.),
            dragging: false,
        });
        self.read_the_selected_text(cx);
        cx.notify();
    }

    fn rotate(&mut self, cx: &mut Context<Self>) {
        self.turn_by(1, cx);
    }

    fn rotate_back(&mut self, cx: &mut Context<Self>) {
        self.turn_by(3, cx);
    }

    fn turn_by(&mut self, quarter_turns: u8, cx: &mut Context<Self>) {
        self.quarter_turns = (self.quarter_turns + quarter_turns) % 4;
        self.thumbnails = vec![None; self.thumbnails.len()];
        self.thumbnails_asked_for.clear();
        // A turned page is a different shape, so a fit worked out for the old one
        // no longer holds.
        self.fitted_for = None;
        self.pages = vec![None; self.pages.len()];
        self.asked_for.clear();
        self.given_another_go.clear();
        self.selection = None;
        self.selected_text = None;
        let wanted = self.pages_worth_rendering();
        self.ask_for_pages(wanted, cx);
        cx.notify();
    }

    /// Looks for `needle` in every page and remembers which pages hold it. The
    /// whole document is read, so it happens off the interface thread.
    fn find(&mut self, needle: SharedString, cx: &mut Context<Self>) {
        if needle.trim().is_empty() {
            self.searched_for = None;
            self.found.clear();
            self.at_match = 0;
            cx.notify();
            return;
        }
        let path = self.path.clone();
        let pages = self.pages.len();
        let looking_for = needle.to_string();
        let match_case = self.match_case;
        let whole_words = self.whole_words;
        self.searched_for = Some(needle);
        let reading = self.reading;
        self.searching = Some(cx.spawn(async move |view, cx| {
            let found = cx
                .background_spawn(async move {
                    let mut found = Vec::new();
                    for page in 0..pages {
                        // Where each one sits, not merely which page holds it: a
                        // reader looking for a word wants to see it marked on the
                        // page, and to step from one to the next.
                        let places = pdf_engine::places_found_on_page(
                            &path,
                            page as PdfPageIndex,
                            &looking_for,
                            match_case,
                            whole_words,
                        )?;
                        for (bottom, left, top, right) in places {
                            found.push(Found {
                                page,
                                bottom,
                                left,
                                top,
                                right,
                            });
                        }
                    }
                    anyhow::Ok(found)
                })
                .await;
            view.update(cx, |view, cx| {
                if view.reading != reading {
                    return;
                }
                match found {
                    Ok(found) => {
                        let first = found.first().map(|found| found.page);
                        view.found = found;
                        view.at_match = 0;
                        if let Some(first) = first {
                            view.show_page(first, cx);
                        }
                    }
                    Err(error) => log::warn!("the PDF could not be searched: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Searches again for what was last looked for, which is what changing how the
    /// search is done has to do.
    fn look_again(&mut self, cx: &mut Context<Self>) {
        if let Some(needle) = self.searched_for.clone() {
            self.find(needle, cx);
        }
    }

    /// Hands the document to whatever prints on this machine. Printing is the
    /// system's business, not the editor's, so the file is passed on rather than
    /// rendered again here.
    fn print(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.background_spawn(async move {
            let printed = smol::process::Command::new("lp").arg(&path).status().await;
            match printed {
                Ok(status) if status.success() => {}
                Ok(status) => log::warn!("printing the PDF ended with {status}"),
                Err(error) => log::warn!("this machine has no `lp` to print with: {error}"),
            }
        })
        .detach();
    }

    /// Writes the document somewhere else, as it is. Nothing is rendered again:
    /// what is saved is the file that was opened, so it stays a document rather
    /// than becoming a picture of one.
    fn save_a_copy(&mut self, cx: &mut Context<Self>) {
        let from = self.path.clone();
        let directory = from
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let suggested = from
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        let asked = cx.prompt_for_new_path(&directory, suggested.as_deref());
        self.saving = Some(cx.spawn(async move |view, cx| {
            let to = match asked.await {
                Ok(Ok(Some(to))) => to,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    log::warn!("choosing where to save the PDF failed: {error:#}");
                    return;
                }
                Err(_) => return,
            };
            let copied = cx
                .background_spawn(async move { smol::fs::copy(&from, &to).await })
                .await;
            if let Err(error) = copied {
                log::warn!("the PDF could not be saved: {error}");
                view.update(cx, |view, cx| {
                    view.trouble = Some(format!("could not save a copy: {error}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Reads what the document says about itself and shows it. Read on asking
    /// rather than on opening: most readers never ask.
    fn show_properties(&mut self, cx: &mut Context<Self>) {
        if self.facts.take().is_some() {
            cx.notify();
            return;
        }
        let path = self.path.clone();
        self.reading_facts = Some(cx.spawn(async move |view, cx| {
            let read = cx
                .background_spawn(async move {
                    let facts = pdf_engine::facts(&path)?;
                    let on_disk = std::fs::metadata(&path).map(|file| file.len()).ok();
                    anyhow::Ok((facts, on_disk, path))
                })
                .await;
            view.update(cx, |view, cx| {
                match read {
                    Ok((facts, on_disk, path)) => {
                        let mut said = Vec::new();
                        said.push(format!("File: {}", path.display()));
                        if let Some(bytes) = on_disk {
                            said.push(format!("Size: {}", in_kilobytes(bytes)));
                        }
                        said.push(format!("Pages: {}", facts.pages));
                        if let Some(page) = facts.first_page {
                            said.push(format!(
                                "Page size: {:.0} x {:.0} pt",
                                page.width, page.height
                            ));
                        }
                        for (name, value) in [
                            ("Title", facts.title),
                            ("Author", facts.author),
                            ("Produced by", facts.producer),
                            ("Created", facts.created),
                        ] {
                            if let Some(value) = value {
                                said.push(format!("{name}: {value}"));
                            }
                        }
                        view.facts = Some(said.join("\n").into());
                    }
                    Err(error) => {
                        log::warn!("the PDF would not say what it is: {error:#}");
                        view.facts = Some(format!("nothing could be read: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Turns between a column of pages and one page at a time. What was being
    /// read stays being read either way round.
    /// Shows the document as a slideshow: one page at a time, the whole page in
    /// view, the window given over to it and nothing else on screen.
    fn present(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.presenting {
            return;
        }
        self.before_presenting = Some((self.one_page_at_a_time, self.fit, self.sidebar));
        self.presenting = true;
        self.blanked = None;
        self.sidebar = Sidebar::Hidden;
        self.show_one_page_at_a_time(true, cx);
        self.fit_to(Fit::Page, cx);
        window.toggle_fullscreen();
        cx.notify();
    }

    /// Puts back what the reader was looking at before the slideshow.
    fn stop_presenting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.presenting {
            return;
        }
        self.presenting = false;
        self.blanked = None;
        if let Some((one_page, fit, sidebar)) = self.before_presenting.take() {
            self.sidebar = sidebar;
            self.show_one_page_at_a_time(one_page, cx);
            self.fit_to(fit, cx);
        }
        window.toggle_fullscreen();
        cx.notify();
    }

    fn blank_the_screen(&mut self, with: Blank, cx: &mut Context<Self>) {
        if !self.presenting {
            return;
        }
        // The same key again brings the slide back, which is how a presenter uses
        // it: press to hide, press to carry on.
        self.blanked = match self.blanked == Some(with) {
            true => None,
            false => Some(with),
        };
        cx.notify();
    }

    /// Turns the pages' colours over, or back.
    fn read_at_night(&mut self, at_night: bool, cx: &mut Context<Self>) {
        if self.at_night == at_night {
            return;
        }
        self.at_night = at_night;
        self.pages = vec![None; self.pages.len()];
        self.asked_for.clear();
        self.given_another_go.clear();
        self.thumbnails = vec![None; self.thumbnails.len()];
        self.thumbnails_asked_for.clear();
        let wanted = self.pages_worth_rendering();
        self.ask_for_pages(wanted, cx);
        cx.notify();
    }

    fn show_one_page_at_a_time(&mut self, one_at_a_time: bool, cx: &mut Context<Self>) {
        if self.one_page_at_a_time == one_at_a_time {
            return;
        }
        let was_reading = self.page_in_view();
        self.one_page_at_a_time = one_at_a_time;
        self.showing = was_reading;
        self.selection = None;
        self.selected_text = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        // Where the pages were is about to stop being true, and a scroll worked
        // out from the old places would land anywhere.
        for cell in &self.page_bounds {
            cell.set(Bounds::default());
        }
        // Back in the column the page being read has to be scrolled to, and
        // where it lands is not known until the column has been laid out. So it
        // is asked for and carried out on the frame that knows.
        self.bring_into_view = (!one_at_a_time).then_some(was_reading);
        let wanted = self.pages_worth_rendering();
        self.ask_for_pages(wanted, cx);
        cx.notify();
    }

    fn show_sidebar(&mut self, which: Sidebar, cx: &mut Context<Self>) {
        self.sidebar = match self.sidebar == which {
            true => Sidebar::Hidden,
            false => which,
        };
        if self.sidebar == Sidebar::Outline && self.outline.is_empty() {
            self.read_the_outline(cx);
        }
        cx.notify();
    }

    fn read_the_outline(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        self.reading_outline = Some(cx.spawn(async move |view, cx| {
            let read = cx
                .background_spawn(async move {
                    let lines = pdf_engine::outline(&path)?;
                    anyhow::Ok(
                        lines
                            .into_iter()
                            .map(|line| Outline {
                                title: line.title.into(),
                                page: line.page,
                                depth: line.depth,
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .await;
            view.update(cx, |view, cx| {
                match read {
                    Ok(lines) => view.outline = lines,
                    Err(error) => log::warn!("the PDF's contents could not be read: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Draws the small pictures the side list shows. They are drawn once at a
    /// fixed width, so zooming leaves them alone.
    fn ask_for_thumbnails(&mut self, wanted: std::ops::Range<usize>, cx: &mut Context<Self>) {
        if self.thumbnails.len() != self.pages.len() {
            self.thumbnails = vec![None; self.pages.len()];
            self.thumbnails_asked_for.clear();
        }
        for index in wanted.start..wanted.end.min(self.thumbnails.len()) {
            if self.thumbnails[index].is_some() || !self.thumbnails_asked_for.insert(index) {
                continue;
            }
            let path = self.path.clone();
            let turns = self.quarter_turns;
            let at_night = self.at_night;
            let at_density = self.screen_pixels;
            let reading = self.reading;
            let thumbnail_width = (THUMBNAIL_WIDTH as f32 * self.screen_pixels)
                .round()
                .max(1.) as u32;
            self.drawing_thumbnails
                .push(cx.spawn(async move |view, cx| {
                    let drawn = cx
                        .background_spawn(async move {
                            pdf_engine::render_page(
                                &path,
                                index as PdfPageIndex,
                                thumbnail_width,
                                turns,
                                at_night,
                            )
                        })
                        .await;
                    view.update(cx, |view, cx| {
                        // Drawn the other way up, or the other way round in
                        // colour, from how the pages are now: it would sit in the
                        // side list disagreeing with them.
                        if view.reading != reading
                            || view.quarter_turns != turns
                            || view.at_night != at_night
                            || (view.screen_pixels - at_density).abs() > 0.01
                        {
                            return;
                        }
                        match drawn {
                            Ok(page) => {
                                if let Some(slot) = view.thumbnails.get_mut(index) {
                                    *slot = Some(Arc::new(as_render_image(page)));
                                }
                            }
                            Err(error) => {
                                // Left as it is until the pages are turned: a
                                // small picture is worth no second attempt of
                                // its own, and asking again at once would spin.
                                log::warn!("page {index} drew no thumbnail: {error:#}");
                            }
                        }
                        cx.notify();
                    })
                    .ok();
                }));
        }
        self.drawing_thumbnails.retain(|task| !task.is_ready());
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.take().is_some() || self.selected_text.take().is_some() {
            cx.notify();
        }
    }

    /// Where a position in the window falls on a page, as a fraction of it. Kept
    /// as fractions rather than pixels so the selection stays on the words it was
    /// drawn around while the document is scrolled or zoomed.
    fn position_on_page(&self, page: usize, position: Point<Pixels>) -> Option<Point<f32>> {
        let bounds = self.page_bounds.get(page)?.get();
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        if width <= 0. || height <= 0. {
            return None;
        }
        Some(point(
            (f32::from(position.x - bounds.origin.x) / width).clamp(0., 1.),
            (f32::from(position.y - bounds.origin.y) / height).clamp(0., 1.),
        ))
    }

    /// Asks the engine for the text under the finished selection. Reading a page
    /// means opening the document again, so it happens off the interface thread
    /// like every other errand to the engine.
    /// The characters the selection covers, in the order the document holds them.
    /// None while the page's characters have not been read, or when the drag was
    /// nowhere near any text.
    fn selected_characters(&self) -> Option<(usize, std::ops::Range<usize>)> {
        let selection = self.selection.as_ref()?;
        let characters = self.chars.get(&selection.page)?;
        let page = self.page_sizes.get(selection.page)?;
        if characters.is_empty() || page.width <= 0. || page.height <= 0. {
            return None;
        }
        let in_points = |at: Point<f32>| {
            let laid_out = as_the_document_lays_it_out(at, self.quarter_turns);
            // Page points count up from the bottom; fractions count down from the
            // top.
            (laid_out.x * page.width, (1. - laid_out.y) * page.height)
        };
        let from = character_nearest(characters, in_points(selection.from))?;
        let to = character_nearest(characters, in_points(selection.to))?;
        let (first, last) = match from <= to {
            true => (from, to),
            false => (to, from),
        };
        Some((selection.page, first..last + 1))
    }

    /// Takes the text the selection covers from the characters already read. No
    /// errand: what is marked and what is copied then cannot disagree.
    fn read_the_selected_text(&mut self, cx: &mut Context<Self>) {
        let text = self.selected_characters().and_then(|(page, covered)| {
            let characters = self.chars.get(&page)?;
            let text: String = characters
                .get(covered)?
                .iter()
                .map(|character| character.character)
                .collect();
            match text.trim().is_empty() {
                true => None,
                false => Some(SharedString::from(text)),
            }
        });
        self.selected_text = text;
        cx.notify();
    }

    fn copy_the_selection(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.selected_text.clone() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    }

    /// Copies everything on the page the pointer was last over.
    fn copy_whole_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let Some(page_size) = self.page_sizes.get(page) else {
            return;
        };
        let (width, height) = (page_size.width, page_size.height);
        let path = self.path.clone();
        self.reading_text = Some(cx.spawn(async move |view, cx| {
            let read = cx
                .background_spawn(async move {
                    pdf_engine::text_in_rect(&path, page as PdfPageIndex, 0., 0., height, width)
                })
                .await;
            view.update(cx, |_view, cx| {
                match read {
                    Ok(text) if !text.trim().is_empty() => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    Ok(_) => log::info!("page {page} of the PDF holds no text to copy"),
                    Err(error) => log::warn!("the PDF's text could not be read: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.page_under(event.position);
        let has_selection = self.selected_text.is_some();
        let view = cx.entity();
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            let for_copy = view.clone();
            let for_page = view.clone();
            let for_in = view.clone();
            let for_out = view.clone();
            let for_reset = view.clone();
            menu.when(has_selection, |menu| {
                menu.entry("Copy Selection", None, move |_, cx| {
                    for_copy.update(cx, |view, cx| view.copy_the_selection(cx));
                })
            })
            .entry("Copy Page Text", None, move |_, cx| {
                if let Some(page) = page {
                    for_page.update(cx, |view, cx| view.copy_whole_page(page, cx));
                }
            })
            .separator()
            .entry("Zoom In", None, move |_, cx| {
                for_in.update(cx, |view, cx| view.zoom_by(ZOOM_STEP, cx));
            })
            .entry("Zoom Out", None, move |_, cx| {
                for_out.update(cx, |view, cx| view.zoom_by(-ZOOM_STEP, cx));
            })
            .entry("Actual Size", None, move |_, cx| {
                for_reset.update(cx, |view, cx| {
                    let back_to_one = 1. - view.zoom;
                    view.zoom_by(back_to_one, cx);
                });
            })
        });
        let dismissed = cx.subscribe(&menu, |view, _, _: &DismissEvent, cx| {
            view.context_menu = None;
            cx.notify();
        });
        self.context_menu = Some((menu, event.position, dismissed));
        cx.notify();
    }

    fn dismiss_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    fn render_menu(&self) -> Option<AnyElement> {
        let (menu, at, _) = self.context_menu.as_ref()?;
        Some(
            deferred(
                anchored()
                    .position(*at)
                    .anchor(Anchor::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }

    /// The strip above the document: how big it is being read at, and the two
    /// buttons that change it.
    fn render_controls(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pages = self.pages.len();
        let matches = self.found.len();
        // Nothing has been painted before the first frame, and a strip that left
        // everything out then would flash on the second.
        let room = match f32::from(self.room_for_controls.get()) {
            nothing if nothing <= 0. => ROOM_FOR_EVERYTHING,
            measured => measured,
        };
        let everything = room >= ROOM_FOR_EVERYTHING;
        let searching = room >= ROOM_FOR_SEARCHING;
        let side_lists = room >= ROOM_FOR_SIDE_LISTS;
        let page_numbers = room >= ROOM_FOR_PAGE_NUMBERS;
        let zoom_label = room >= ROOM_FOR_A_ZOOM_LABEL;
        let width_of_the_strip = self.room_for_controls.clone();
        h_flex()
            .relative()
            .flex_none()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            // The controls sit together in the middle of the strip: a row of them
            // along one edge of a wide window reads as left over from something.
            .justify_center()
            .overflow_x_hidden()
            .border_b_1()
            .border_color(ui::cyberpunk::border_dim())
            .child(
                canvas(
                    move |bounds, _, _| width_of_the_strip.set(bounds.size.width),
                    |_, _, _, _| (),
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .when(side_lists, |strip| {
                strip
                    .child(self.tool(
                        "pdf-thumbnails",
                        IconName::Image,
                        "Page thumbnails",
                        self.sidebar == Sidebar::Thumbnails,
                        cx.listener(|view, _, _, cx| view.show_sidebar(Sidebar::Thumbnails, cx)),
                    ))
                    .child(self.tool(
                        "pdf-outline",
                        IconName::ListTree,
                        "Contents",
                        self.sidebar == Sidebar::Outline,
                        cx.listener(|view, _, _, cx| view.show_sidebar(Sidebar::Outline, cx)),
                    ))
                    .child(self.divider())
            })
            .child(self.tool(
                "pdf-zoom-out",
                IconName::Dash,
                "Zoom out",
                false,
                cx.listener(|view, _, _, cx| view.zoom_by(-ZOOM_STEP, cx)),
            ))
            .when(zoom_label, |strip| {
                strip.child(self.render_zoom_choices(cx))
            })
            .child(self.tool(
                "pdf-zoom-in",
                IconName::Plus,
                "Zoom in",
                false,
                cx.listener(|view, _, _, cx| view.zoom_by(ZOOM_STEP, cx)),
            ))
            .when(searching, |strip| {
                strip
                    .child(self.tool(
                        "pdf-fit-width",
                        IconName::ArrowRightLeft,
                        "Fit the width",
                        self.fit == Fit::Width,
                        cx.listener(|view, _, _, cx| view.fit_to(Fit::Width, cx)),
                    ))
                    .child(self.tool(
                        "pdf-fit-page",
                        IconName::Maximize,
                        "Fit the page",
                        self.fit == Fit::Page,
                        cx.listener(|view, _, _, cx| view.fit_to(Fit::Page, cx)),
                    ))
            })
            .child(self.divider())
            .when(everything, |strip| {
                strip.child(self.tool(
                    "pdf-first",
                    IconName::ChevronUpDown,
                    "First page",
                    false,
                    cx.listener(|view, _, _, cx| view.show_page(0, cx)),
                ))
            })
            .when(page_numbers, |strip| {
                strip
                    .child(self.tool(
                        "pdf-previous",
                        IconName::ChevronUp,
                        "Previous page",
                        false,
                        cx.listener(|view, _, _, cx| view.step_page(-1, cx)),
                    ))
                    .child(
                        div()
                            .flex_none()
                            .w(px(46.))
                            .h(px(22.))
                            .px_1()
                            .border_1()
                            .border_color(ui::cyberpunk::border_dim())
                            .child(self.page_field.clone()),
                    )
            })
            .when(side_lists, |strip| {
                strip.child(
                    Label::new(format!("of {pages}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(page_numbers, |strip| {
                strip.child(self.tool(
                    "pdf-next",
                    IconName::ChevronDown,
                    "Next page",
                    false,
                    cx.listener(|view, _, _, cx| view.step_page(1, cx)),
                ))
            })
            .when(everything, |strip| {
                strip.child(self.tool(
                    "pdf-last",
                    IconName::ChevronUpDown,
                    "Last page",
                    false,
                    cx.listener(move |view, _, _, cx| view.show_page(pages.saturating_sub(1), cx)),
                ))
            })
            .when(searching, |strip| {
                strip
                    .child(
                        div()
                            .flex_none()
                            .w(px(if everything { 180. } else { 120. }))
                            .child(self.render_find_field(window, cx)),
                    )
                    .children(self.searched_for.clone().map(|needle| {
                        Label::new(match matches {
                            0 => format!("no {needle}"),
                            found => format!("{} of {found}", self.at_match + 1),
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                    }))
                    .child(self.tool(
                        "pdf-find-previous",
                        IconName::ChevronLeft,
                        "Previous match",
                        false,
                        cx.listener(|view, _, _, cx| view.step_match(-1, cx)),
                    ))
                    .child(self.tool(
                        "pdf-find-next",
                        IconName::ChevronRight,
                        "Next match",
                        false,
                        cx.listener(|view, _, _, cx| view.step_match(1, cx)),
                    ))
            })
            .child(self.divider())
            .child(self.render_more_menu(cx))
            .into_any_element()
    }

    /// The zoom, and a list of the sizes a reader asks for by name. Typing a
    /// number is not what anybody does here; picking 100% or "fit the page" is.
    fn render_zoom_choices(&self, cx: &mut Context<Self>) -> AnyElement {
        let showing = format!("{:.0}%", self.zoom * 100.);
        div()
            .flex_none()
            .debug_selector(|| "pdf-zoom".to_string())
            .child(
                ui::PopoverMenu::new("pdf-zoom-choices")
                    .trigger(
                        Button::new("pdf-zoom-reset", showing)
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("The size pages are shown at")),
                    )
                    .menu({
                        let view = cx.entity();
                        move |window, cx| {
                            let view = view.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, cx| {
                                let fit = view.read(cx).fit;
                                let width = view.clone();
                                let page = view.clone();
                                let mut menu = menu
                                    .toggleable_entry(
                                        "Fit the width",
                                        fit == Fit::Width,
                                        ui::IconPosition::Start,
                                        None,
                                        move |_, cx| {
                                            width
                                                .update(cx, |view, cx| view.fit_to(Fit::Width, cx));
                                        },
                                    )
                                    .toggleable_entry(
                                        "Fit the page",
                                        fit == Fit::Page,
                                        ui::IconPosition::Start,
                                        None,
                                        move |_, cx| {
                                            page.update(cx, |view, cx| view.fit_to(Fit::Page, cx));
                                        },
                                    )
                                    .separator();
                                for at in ZOOM_CHOICES {
                                    let view = view.clone();
                                    menu = menu.entry(
                                        format!("{:.0}%", at * 100.),
                                        None,
                                        move |_, cx| {
                                            view.update(cx, |view, cx| {
                                                view.fit = Fit::Free;
                                                view.set_zoom(at, cx);
                                            });
                                        },
                                    );
                                }
                                menu
                            }))
                        }
                    }),
            )
            .into_any_element()
    }

    /// One button of the strip. Marked when what it does is what is being done,
    /// since fitting and the side lists stay on once pressed.
    fn tool(
        &self,
        id: &'static str,
        icon: IconName,
        says: &'static str,
        on: bool,
        pressed: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .flex_none()
            .debug_selector(|| id.to_string())
            .child(
                IconButton::new(id, icon)
                    .icon_size(IconSize::Small)
                    .toggle_state(on)
                    .tooltip(Tooltip::text(says))
                    .on_click(pressed),
            )
            .into_any_element()
    }

    fn divider(&self) -> AnyElement {
        div()
            .flex_none()
            .w(px(1.))
            .h(px(16.))
            .mx_1()
            .bg(ui::cyberpunk::border_dim())
            .into_any_element()
    }

    /// Follows the pointer for as long as a selection is being dragged, wherever
    /// it goes. An element is only told about the moves made over it, so without
    /// this a drag stops the moment it leaves the page -- which is where most
    /// drags end.
    fn follow_the_drag(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dragging = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.dragging);
        if !dragging && self.panning.is_none() {
            return None;
        }
        let view = cx.entity().downgrade();
        Some(
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let moving = view.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        match event.pressed_button {
                            Some(MouseButton::Left) => {
                                moving
                                    .update(cx, |view, cx| view.keep_selecting(event, cx))
                                    .ok();
                            }
                            // The middle button drags what is shown, the way a
                            // hand on the page would.
                            Some(MouseButton::Middle) => {
                                moving
                                    .update(cx, |view, cx| view.keep_panning(event, cx))
                                    .ok();
                            }
                            _ => {}
                        }
                    });
                    let letting_go = view;
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        match event.button {
                            MouseButton::Left => {
                                letting_go
                                    .update(cx, |view, cx| view.finish_selecting(event, cx))
                                    .ok();
                            }
                            MouseButton::Middle => {
                                letting_go
                                    .update(cx, |view, cx| {
                                        view.panning = None;
                                        cx.notify();
                                    })
                                    .ok();
                            }
                            _ => {}
                        }
                    });
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .into_any_element(),
        )
    }

    /// Drags what is shown under the pointer, which is what a hand on a page does.
    fn keep_panning(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(was_at) = self.panning else {
            return;
        };
        let moved = event.position - was_at;
        self.panning = Some(event.position);
        let at = self.scroll.offset();
        self.scroll
            .set_offset(point(at.x + moved.x, at.y + moved.y));
        cx.notify();
    }

    /// What a reader reaches for rarely. Kept behind one button so the strip
    /// stays inside a narrow pane.
    fn render_more_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .debug_selector(|| "pdf-more".to_string())
            .child(
                ui::PopoverMenu::new("pdf-more")
                    .trigger(
                        IconButton::new("pdf-more-button", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("More")),
                    )
                    .menu({
                        let view = cx.entity();
                        move |window, cx| {
                            let view = view.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, cx| {
                                let one_at_a_time = view.read(cx).one_page_at_a_time;
                                let at_night = view.read(cx).at_night;
                                let two_across = view.read(cx).two_across;
                                let find = view.clone();
                                let fit_width = view.clone();
                                let fit_page = view.clone();
                                let one_page = view.clone();
                                let book = view.clone();
                                let night = view.clone();
                                let present = view.clone();
                                let onwards = view.clone();
                                let backwards = view.clone();
                                let turn = view.clone();
                                let turn_back = view.clone();
                                let select = view.clone();
                                let copy = view.clone();
                                let save = view.clone();
                                let print = view.clone();
                                let facts = view;
                                menu.entry("Find in the document", None, move |window, cx| {
                                    find.update(cx, |view, cx| {
                                        view.searching_now = true;
                                        view.find_editor.focus_handle(cx).focus(window, cx);
                                        cx.notify();
                                    });
                                })
                                .entry("Fit the width", None, move |_, cx| {
                                    fit_width.update(cx, |view, cx| view.fit_to(Fit::Width, cx));
                                })
                                .entry("Fit the page", None, move |_, cx| {
                                    fit_page.update(cx, |view, cx| view.fit_to(Fit::Page, cx));
                                })
                                .separator()
                                .entry("Turn right", None, move |_, cx| {
                                    turn.update(cx, |view, cx| view.rotate(cx));
                                })
                                .entry("Turn left", None, move |_, cx| {
                                    turn_back.update(cx, |view, cx| view.rotate_back(cx));
                                })
                                .separator()
                                .toggleable_entry(
                                    "One page at a time",
                                    one_at_a_time,
                                    ui::IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        one_page.update(cx, |view, cx| {
                                            let now = !view.one_page_at_a_time;
                                            view.show_one_page_at_a_time(now, cx);
                                        });
                                    },
                                )
                                .toggleable_entry(
                                    "Two pages across",
                                    two_across,
                                    ui::IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        book.update(cx, |view, cx| {
                                            let now = !view.two_across;
                                            view.show_two_across(now, cx);
                                        });
                                    },
                                )
                                .toggleable_entry(
                                    "Night mode",
                                    at_night,
                                    ui::IconPosition::Start,
                                    None,
                                    move |_, cx| {
                                        night.update(cx, |view, cx| {
                                            let now = !view.at_night;
                                            view.read_at_night(now, cx);
                                        });
                                    },
                                )
                                .entry("Present as a slideshow", None, move |window, cx| {
                                    present.update(cx, |view, cx| view.present(window, cx));
                                })
                                .separator()
                                .entry("Next page", None, move |_, cx| {
                                    onwards.update(cx, |view, cx| view.step_page(1, cx));
                                })
                                .entry("Previous page", None, move |_, cx| {
                                    backwards.update(cx, |view, cx| view.step_page(-1, cx));
                                })
                                .separator()
                                .entry("Select the page", None, move |_, cx| {
                                    select.update(cx, |view, cx| view.select_the_page(cx));
                                })
                                .entry("Copy the selection", None, move |_, cx| {
                                    copy.update(cx, |view, cx| view.copy_the_selection(cx));
                                })
                                .separator()
                                .entry("Save a copy", None, move |_, cx| {
                                    save.update(cx, |view, cx| view.save_a_copy(cx));
                                })
                                .entry("Print", None, move |_, cx| {
                                    print.update(cx, |view, cx| view.print(cx));
                                })
                                .entry("Properties", None, move |_, cx| {
                                    facts.update(cx, |view, cx| view.show_properties(cx));
                                })
                                .separator()
                                .entry(
                                    "Full screen",
                                    None,
                                    |window, _| {
                                        window.toggle_fullscreen();
                                    },
                                )
                            }))
                        }
                    }),
            )
            .into_any_element()
    }

    /// The side list: either small pictures of the pages or the document's own
    /// contents. Both take the reader to a page.
    fn render_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.sidebar == Sidebar::Hidden
            || f32::from(self.room_for_controls.get()) < ROOM_FOR_SIDE_LISTS
        {
            return None;
        }
        let showing = self.page_in_view();
        let inside: Vec<AnyElement> = match self.sidebar {
            Sidebar::Hidden => Vec::new(),
            Sidebar::Thumbnails => (0..self.pages.len())
                .map(|index| {
                    let drawn = self.thumbnails.get(index).cloned().flatten();
                    v_flex()
                        .id(("pdf-thumbnail", index))
                        .items_center()
                        .gap_1()
                        .p_1()
                        .when(index == showing, |this| {
                            this.bg(ui::cyberpunk::row_chosen())
                        })
                        .hover(|this| this.bg(ui::cyberpunk::row_hovered()))
                        .cursor_pointer()
                        .on_click(cx.listener(move |view, _, _, cx| view.show_page(index, cx)))
                        .child(match drawn {
                            Some(page) => img(gpui::ImageSource::Render(page))
                                .w(px(THUMBNAIL_WIDTH as f32))
                                .into_any_element(),
                            None => div()
                                .w(px(THUMBNAIL_WIDTH as f32))
                                .h(px(160.))
                                .border_1()
                                .border_color(ui::cyberpunk::border_dim())
                                .into_any_element(),
                        })
                        .child(
                            Label::new(format!("{}", index + 1))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element()
                })
                .collect(),
            Sidebar::Outline => self
                .outline
                .iter()
                .enumerate()
                .map(|(line, entry)| {
                    let page = entry.page;
                    div()
                        .id(("pdf-outline", line))
                        .w_full()
                        .px_1()
                        .py_0p5()
                        .pl(px(4. + 10. * entry.depth.min(6) as f32))
                        .hover(|this| this.bg(ui::cyberpunk::row_hovered()))
                        .cursor_pointer()
                        .on_click(cx.listener(move |view, _, _, cx| view.show_page(page, cx)))
                        .child(Label::new(entry.title.clone()).size(LabelSize::Small))
                        .into_any_element()
                })
                .collect(),
        };
        let empty = inside.is_empty();
        Some(
            div()
                .id("pdf-sidebar")
                .flex_none()
                .w(px(168.))
                .h_full()
                .border_r_1()
                .border_color(ui::cyberpunk::border_dim())
                .overflow_y_scroll()
                .track_scroll(&self.sidebar_scroll)
                .child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .p_1()
                        .when(empty, |this| {
                            this.child(
                                Label::new("nothing to show")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .children(inside),
                )
                .custom_scrollbars(
                    ui::Scrollbars::always_visible(ui::ScrollAxes::Vertical)
                        .tracked_scroll_handle(&self.sidebar_scroll),
                    window,
                    cx,
                )
                .into_any_element(),
        )
    }

    /// What the document says about itself, over the page until it is dismissed.
    fn render_properties(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let said = self.facts.clone()?;
        Some(
            div()
                .absolute()
                .top(px(48.))
                .right(px(24.))
                .w(px(360.))
                .p_3()
                .elevation_2(cx)
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .child(Label::new("Document").size(LabelSize::Small))
                                .child(
                                    IconButton::new("pdf-properties-close", IconName::Close)
                                        .icon_size(IconSize::XSmall)
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.facts = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .children(said.split('\n').map(|line| {
                            Label::new(line.to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        })),
                )
                .into_any_element(),
        )
    }

    /// The field text is looked for in. Enter starts the search; the field keeps
    /// what was typed so the same words can be looked for again.
    fn render_find_field(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .h(px(24.))
            .on_action(cx.listener(|view, _: &::menu::Confirm, _, cx| {
                let needle: SharedString = view.find_editor.read(cx).text(cx).into();
                view.find(needle, cx);
            }))
            .px_1()
            .border_1()
            .border_color(ui::cyberpunk::border_dim())
            .child(div().flex_1().child(self.find_editor.clone()))
            .child(
                IconButton::new("pdf-find-case", IconName::CaseSensitive)
                    .icon_size(IconSize::XSmall)
                    .toggle_state(self.match_case)
                    .tooltip(Tooltip::text("Mind the case"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.match_case = !view.match_case;
                        view.look_again(cx);
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("pdf-find-words", IconName::WholeWord)
                    .icon_size(IconSize::XSmall)
                    .toggle_state(self.whole_words)
                    .tooltip(Tooltip::text("Whole words only"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.whole_words = !view.whole_words;
                        view.look_again(cx);
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("pdf-find-go", IconName::MagnifyingGlass)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Find in document"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        let needle: SharedString = view.find_editor.read(cx).text(cx).into();
                        view.find(needle, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_page_placeholder(&self, index: usize) -> AnyElement {
        let width = px(BASE_PAGE_WIDTH * self.zoom);
        let height = match self.page_sizes.get(index) {
            Some(size) if size.width > 0. => {
                px(BASE_PAGE_WIDTH * self.zoom * (size.height / size.width))
            }
            _ => px(BASE_PAGE_WIDTH * self.zoom * 1.414),
        };
        div()
            .w(width)
            .h(height)
            .bg(ui::cyberpunk::canvas())
            .border_1()
            .border_color(ui::cyberpunk::border_dim())
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Label::new(format!("Page {}", index + 1)).color(Color::Muted)),
            )
            .into_any_element()
    }

    /// The rectangle being dragged, drawn inside the page it belongs to so it
    /// travels with the page rather than staying where the window was.
    /// Marks every place the search found on this page, with the one the reader is
    /// standing on marked more strongly. A search that shows only a count leaves
    /// the reader hunting the page for the word themselves.
    fn render_found_on(&self, page: usize) -> Option<AnyElement> {
        if self.found.is_empty() {
            return None;
        }
        let bounds = self.page_bounds.get(page)?.get();
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        let page_size = self.page_sizes.get(page)?;
        if width <= 0. || height <= 0. || page_size.width <= 0. || page_size.height <= 0. {
            return None;
        }
        let marks = self
            .found
            .iter()
            .enumerate()
            .filter(|(_, found)| found.page == page)
            .map(|(at, found)| {
                let corners = [
                    point(
                        found.left / page_size.width,
                        1. - found.top / page_size.height,
                    ),
                    point(
                        found.right / page_size.width,
                        1. - found.bottom / page_size.height,
                    ),
                ]
                .map(|at| as_it_is_painted(at, self.quarter_turns));
                let from = point(
                    corners[0].x.min(corners[1].x),
                    corners[0].y.min(corners[1].y),
                );
                let to = point(
                    corners[0].x.max(corners[1].x),
                    corners[0].y.max(corners[1].y),
                );
                let standing_on = at == self.at_match;
                div()
                    .absolute()
                    .left(px(from.x * width))
                    .top(px(from.y * height))
                    .w(px((to.x - from.x) * width))
                    .h(px((to.y - from.y) * height))
                    .bg(match standing_on {
                        true => ui::cyberpunk::Accent::Red.border().opacity(0.45),
                        false => ui::cyberpunk::Accent::Red.border().opacity(0.20),
                    })
            })
            .collect::<Vec<_>>();
        if marks.is_empty() {
            return None;
        }
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .debug_selector(|| "pdf-found".to_string())
                .children(marks)
                .into_any_element(),
        )
    }

    /// The mark over the text a drag covers: a band for each line of it. A box
    /// around the whole drag would cover what is not selected and would not show
    /// where a line ends.
    fn render_selection_on(&self, page: usize) -> Option<AnyElement> {
        let selection = self.selection.as_ref()?;
        if selection.page != page {
            return None;
        }
        let bounds = self.page_bounds.get(page)?.get();
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        if width <= 0. || height <= 0. {
            return None;
        }
        let page_size = self.page_sizes.get(page)?;
        if page_size.width <= 0. || page_size.height <= 0. {
            return None;
        }
        let (_, covered) = self.selected_characters()?;
        let characters = self.chars.get(&page)?;
        let bands = bands_over(characters, covered);
        if bands.is_empty() {
            return None;
        }
        let marked = bands.into_iter().map(|(bottom, left, top, right)| {
            // Page points to fractions of the page as laid out, then to fractions
            // of the page as painted, then to where that lands on screen.
            let corners = [
                point(left / page_size.width, 1. - top / page_size.height),
                point(right / page_size.width, 1. - bottom / page_size.height),
            ]
            .map(|at| as_it_is_painted(at, self.quarter_turns));
            let from = point(
                corners[0].x.min(corners[1].x),
                corners[0].y.min(corners[1].y),
            );
            let to = point(
                corners[0].x.max(corners[1].x),
                corners[0].y.max(corners[1].y),
            );
            div()
                .absolute()
                .left(px(from.x * width))
                .top(px(from.y * height))
                .w(px((to.x - from.x) * width))
                .h(px((to.y - from.y) * height))
                .bg(ui::cyberpunk::Accent::Cyan.border().opacity(0.32))
        });
        Some(
            div()
                // Pinned to the page's corner. Absolute with no corner named lands
                // where it would have gone in the flow -- under the picture, off
                // the bottom of the page -- and the marks are drawn where nobody
                // can see them.
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .debug_selector(|| "pdf-selection".to_string())
                .children(marked)
                .into_any_element(),
        )
    }
}

/// Lays a document's pages out two across, the way a book is read.
fn in_rows_of_two(pages: Vec<AnyElement>) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    let mut pages = pages.into_iter();
    loop {
        let left = pages.next();
        let right = pages.next();
        match (left, right) {
            (None, _) => return rows,
            (Some(left), right) => rows.push(
                h_flex()
                    .items_start()
                    .gap_4()
                    .child(left)
                    .children(right)
                    .into_any_element(),
            ),
        }
    }
}

/// A file size the way a reader reads one.
fn in_kilobytes(bytes: u64) -> String {
    match bytes {
        under if under < 1024 => format!("{under} bytes"),
        under if under < 1024 * 1024 => format!("{:.0} KB", bytes as f64 / 1024.),
        _ => format!("{:.1} MB", bytes as f64 / (1024. * 1024.)),
    }
}

fn as_render_image(page: pdf_engine::RenderedPage) -> RenderImage {
    let buffer = image::RgbaImage::from_raw(page.width, page.height, page.pixels)
        .expect("the engine reports the size of the pixels it returned");
    RenderImage::new(SmallVec::from_elem(image::Frame::new(buffer), 1))
}

impl Focusable for PdfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<()> for PdfView {}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Going by where things landed last time: the first frame of a document
        // shows frames, and the one after it has the pages the reader can see.
        self.drawn_for_this_screen(window, cx);
        self.keep_the_fit(cx);
        if let Some(page) = self.bring_into_view {
            // Every page: until the whole column has been laid out the page
            // asked for may still be sitting where the single page was, and the
            // scroll would be worked out from that.
            let laid_out = self
                .page_bounds
                .iter()
                .all(|cell| cell.get().size.height > px(0.));
            if laid_out {
                self.bring_into_view = None;
                self.show_page(page, cx);
            }
        }
        let wanted = self.pages_worth_rendering();
        self.let_go_of_distant_pages(&wanted);
        self.ask_for_pages(wanted.clone(), cx);
        self.ask_for_chars(wanted.clone(), cx);
        self.ask_for_links(wanted, cx);
        if self.sidebar == Sidebar::Thumbnails {
            let showing = self.page_in_view();
            let around = showing.saturating_sub(10)..(showing + 20).min(self.pages.len());
            self.ask_for_thumbnails(around, cx);
        }

        let on_screen: Vec<usize> = match self.one_page_at_a_time {
            true => match self.pages.is_empty() {
                true => Vec::new(),
                false => vec![self.showing.min(self.pages.len() - 1)],
            },
            false => (0..self.pages.len()).collect(),
        };
        let pages: Vec<AnyElement> = on_screen
            .into_iter()
            .map(|index| {
                let drawn = self.pages[index].clone();
                let where_it_lands = self.page_bounds.get(index).cloned();
                div()
                    .relative()
                    .child(match drawn {
                        Some(page) => img(gpui::ImageSource::Render(page))
                            .w(px(BASE_PAGE_WIDTH * self.zoom))
                            .into_any_element(),
                        None => self.render_page_placeholder(index),
                    })
                    .children(self.render_found_on(index))
                    .children(self.render_selection_on(index))
                    // Where this page ended up, recorded without taking any room:
                    // a position in the window means nothing until it can be told
                    // which page it is over and where on it.
                    // Pinned to the page's own corner. An absolutely placed
                    // element with no corner named lands where it would have gone
                    // in the flow -- below the picture -- and then every position
                    // read off it is a page out.
                    .children(where_it_lands.map(|cell| {
                        canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| ())
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                    }))
                    .into_any_element()
            })
            .collect();

        let viewport = self.viewport.clone();
        let last_page = self.pages.len().saturating_sub(1);
        let mut keys = gpui::KeyContext::new_with_defaults();
        keys.add("PdfView");
        if self.presenting {
            // A slideshow is driven by plain keys -- space, the arrows, b and w --
            // which would be typing anywhere else.
            keys.add("presenting");
        }
        v_flex()
            .id("pdf-view")
            .key_context(keys)
            .track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &PdfZoomIn, _, cx| view.zoom_by(ZOOM_STEP, cx)))
            .on_action(cx.listener(|view, _: &PdfZoomOut, _, cx| view.zoom_by(-ZOOM_STEP, cx)))
            .on_action(cx.listener(|view, _: &PdfZoomReset, _, cx| {
                view.fit = Fit::Free;
                view.set_zoom(1., cx);
            }))
            .on_action(cx.listener(|view, _: &PdfFitWidth, _, cx| view.fit_to(Fit::Width, cx)))
            .on_action(cx.listener(|view, _: &PdfFitPage, _, cx| view.fit_to(Fit::Page, cx)))
            .on_action(cx.listener(|view, _: &PdfCopy, _, cx| view.copy_the_selection(cx)))
            .on_action(cx.listener(|view, _: &PdfSelectPage, _, cx| view.select_the_page(cx)))
            .on_action(cx.listener(|view, _: &PdfNextPage, _, cx| view.step_page(1, cx)))
            .on_action(cx.listener(|view, _: &PdfPreviousPage, _, cx| view.step_page(-1, cx)))
            .on_action(cx.listener(|view, _: &PdfFirstPage, _, cx| view.show_page(0, cx)))
            .on_action(
                cx.listener(move |view, _: &PdfLastPage, _, cx| view.show_page(last_page, cx)),
            )
            .on_action(cx.listener(|view, _: &PdfRotate, _, cx| view.rotate(cx)))
            .on_action(cx.listener(|view, _: &PdfRotateBack, _, cx| view.rotate_back(cx)))
            .on_action(cx.listener(|view, _: &PdfPrint, _, cx| view.print(cx)))
            .on_action(cx.listener(|view, _: &PdfSaveACopy, _, cx| view.save_a_copy(cx)))
            .on_action(cx.listener(|view, _: &PdfProperties, _, cx| view.show_properties(cx)))
            .on_action(cx.listener(|view, _: &PdfFindNext, _, cx| view.step_match(1, cx)))
            .on_action(cx.listener(|view, _: &PdfFindPrevious, _, cx| view.step_match(-1, cx)))
            .on_action(|_: &PdfFullScreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|view, _: &PdfThumbnails, _, cx| {
                view.show_sidebar(Sidebar::Thumbnails, cx)
            }))
            .on_action(
                cx.listener(|view, _: &PdfContents, _, cx| view.show_sidebar(Sidebar::Outline, cx)),
            )
            .on_action(cx.listener(|view, _: &PdfOnePage, _, cx| {
                let one_at_a_time = !view.one_page_at_a_time;
                view.show_one_page_at_a_time(one_at_a_time, cx);
            }))
            .on_action(cx.listener(|view, _: &PdfNightMode, _, cx| {
                let at_night = !view.at_night;
                view.read_at_night(at_night, cx);
            }))
            .on_action(cx.listener(|view, _: &PdfTenPagesOn, _, cx| view.step_page(10, cx)))
            .on_action(cx.listener(|view, _: &PdfTenPagesBack, _, cx| view.step_page(-10, cx)))
            .on_action(
                cx.listener(|view, _: &PdfPresent, window, cx| match view.presenting {
                    true => view.stop_presenting(window, cx),
                    false => view.present(window, cx),
                }),
            )
            .on_action(cx.listener(|view, _: &PdfBlackScreen, _, cx| {
                view.blank_the_screen(Blank::Black, cx)
            }))
            .on_action(cx.listener(|view, _: &PdfWhiteScreen, _, cx| {
                view.blank_the_screen(Blank::White, cx)
            }))
            .on_action(cx.listener(|view, _: &PdfTwoAcross, _, cx| {
                let two_across = !view.two_across;
                view.show_two_across(two_across, cx);
            }))
            .on_action(cx.listener(|view, _: &PdfGoToPage, window, cx| {
                view.page_field.focus_handle(cx).focus(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &PdfFind, window, cx| {
                view.searching_now = true;
                view.find_editor.focus_handle(cx).focus(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &::menu::Cancel, window, cx| {
                // A slideshow is what Escape leaves first: everything else it
                // does can wait until the reader is back in the document.
                if view.blanked.take().is_some() {
                    cx.notify();
                    return;
                }
                if view.presenting {
                    view.stop_presenting(window, cx);
                    return;
                }
                view.dismiss_menu(cx);
                view.clear_selection(cx);
                if view.facts.take().is_some() {
                    cx.notify();
                }
            }))
            .size_full()
            .bg(match self.presenting {
                true => gpui::black(),
                false => ui::cyberpunk::surface(),
            })
            .children(match self.presenting {
                true => None,
                false => Some(self.render_controls(window, cx)),
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .children(self.render_sidebar(window, cx))
                    .child(
                        // The window the pages are read through, measured from
                        // outside the scrolling: everything inside is moved by
                        // the scroll as it is painted, so a rectangle measured in
                        // there says where the window was, not where it is.
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                canvas(move |bounds, _, _| viewport.set(bounds), |_, _, _, _| ())
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full(),
                            )
                            .child(
                                div()
                                    .id("pdf-pages")
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .p_4()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, event: &MouseDownEvent, window, cx| {
                                            view.start_selecting(event, window, cx)
                                        }),
                                    )
                                    .on_mouse_move(cx.listener(
                                        |view, event: &MouseMoveEvent, _, cx| {
                                            view.keep_selecting(event, cx)
                                        },
                                    ))
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(|view, event: &MouseUpEvent, _, cx| {
                                            view.finish_selecting(event, cx)
                                        }),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Middle,
                                        cx.listener(|view, event: &MouseDownEvent, _, cx| {
                                            view.panning = Some(event.position);
                                            cx.notify();
                                        }),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(|view, event: &MouseDownEvent, window, cx| {
                                            view.open_menu(event, window, cx);
                                            cx.stop_propagation();
                                        }),
                                    )
                                    // Held down, the wheel zooms rather than
                                    // scrolls, as it does in every other reader.
                                    .on_scroll_wheel(cx.listener(
                                        |view, event: &gpui::ScrollWheelEvent, window, cx| {
                                            if !event.modifiers.secondary() {
                                                return;
                                            }
                                            let by =
                                                event.delta.pixel_delta(window.line_height()).y;
                                            if by == px(0.) {
                                                return;
                                            }
                                            let step = match f32::from(by) > 0. {
                                                true => ZOOM_STEP,
                                                false => -ZOOM_STEP,
                                            };
                                            view.zoom_by(step, cx);
                                            // Or the pages scroll under the zoom
                                            // as well.
                                            cx.stop_propagation();
                                        },
                                    ))
                                    .child(
                                        v_flex()
                                            .items_center()
                                            .gap_4()
                                            .children(self.trouble.clone().map(|trouble| {
                                                Label::new(trouble)
                                                    .color(Color::Error)
                                                    .into_any_element()
                                            }))
                                            .children(match self.asks_for_a_password {
                                                true => vec![self.render_password_prompt(cx)],
                                                // Two across, the way a book is
                                                // read, or one under the other.
                                                false => match self.two_across {
                                                    true => in_rows_of_two(pages),
                                                    false => pages,
                                                },
                                            }),
                                    )
                                    // Always shown rather than fading after a scroll: a
                                    // long document with no visible bar reads as one page.
                                    .custom_scrollbars(
                                        ui::Scrollbars::always_visible(ui::ScrollAxes::Vertical)
                                            .tracked_scroll_handle(&self.scroll),
                                        window,
                                        cx,
                                    ),
                            ),
                    ),
            )
            .children(self.follow_the_drag(cx))
            .children(self.blanked.map(|blank| {
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(match blank {
                        Blank::Black => gpui::black(),
                        Blank::White => gpui::white(),
                    })
            }))
            .children(self.render_properties(cx))
            .children(self.render_menu())
    }
}

impl workspace::ProjectItem for PdfView {
    type Item = PdfItem;

    fn for_project_item(
        _project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, window, cx)
    }
}

impl Item for PdfView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDoc))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    /// The document carries its own strip of controls, so the editor's toolbar
    /// above it would be an empty band taking a row of the window.
    fn show_toolbar(&self) -> bool {
        false
    }

    fn breadcrumb_location(&self, _cx: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A4 in points, near enough: what the engine reports a page's own size to be.
    fn a4() -> pdf_engine::PageSize {
        pdf_engine::PageSize {
            width: 595.,
            height: 842.,
        }
    }

    use gpui::{Bounds, TestAppContext, VisualTestContext, size};
    use workspace::AppState;

    /// What a reader needs before it can be built: settings, a theme and the
    /// editor its two little fields are.
    fn a_working_editor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
        });
    }

    /// A reader with a document of `pages` A4 pages already measured, in a window
    /// of `viewport`. Nothing is rendered: the engine is not there in a test, and
    /// none of what is measured here needs a drawn page.
    fn a_reader_of(
        pages: usize,
        viewport: Bounds<Pixels>,
        cx: &mut TestAppContext,
    ) -> gpui::Entity<PdfView> {
        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        view.update(cx, |view, _| {
            view.pages = vec![None; pages];
            view.page_sizes = (0..pages).map(|_| a4()).collect();
            view.page_bounds = (0..pages)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
            view.viewport.set(viewport);
        });
        view
    }

    fn a_window_of(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width), px(height)),
        }
    }

    #[gpui::test]
    async fn a_page_fitted_to_the_width_fills_the_window(cx: &mut TestAppContext) {
        let view = a_reader_of(3, a_window_of(1200., 400.), cx);
        let (fitted, drawn_width) = view.update(cx, |view, _| {
            let fitted = view.zoom_for(Fit::Width).expect("a width to fit to");
            (fitted, BASE_PAGE_WIDTH * fitted)
        });

        let room = 1200. - FITTING_MARGIN;
        assert!(
            (drawn_width - room).abs() < 1.,
            "a page fitted to a 1200px window drew {drawn_width}px wide, not {room}px"
        );
        // A page fitted to the width may well be taller than the window; that is
        // what scrolling is for, and it is what tells this fit from the other.
        let drawn_height = drawn_width * (a4().height / a4().width);
        assert!(
            drawn_height > 400.,
            "this window is too tall for the test to tell the two fits apart"
        );
        assert!(fitted > 1., "the window is wider than a page's own width");
    }

    #[gpui::test]
    async fn a_page_fitted_to_the_window_is_whole(cx: &mut TestAppContext) {
        let view = a_reader_of(3, a_window_of(1200., 400.), cx);
        let fitted = view.update(cx, |view, _| {
            view.zoom_for(Fit::Page).expect("a page to fit to")
        });

        let drawn_width = BASE_PAGE_WIDTH * fitted;
        let drawn_height = drawn_width * (a4().height / a4().width);
        assert!(
            drawn_height <= 400. + 1.,
            "a whole page has to be in view: it drew {drawn_height}px tall in a 400px window"
        );
        assert!(
            drawn_height > 400. - FITTING_MARGIN - 1.,
            "a page fitted to the window should use the window: it drew {drawn_height}px tall"
        );
    }

    #[gpui::test]
    async fn a_turned_page_is_fitted_by_the_side_it_now_shows(cx: &mut TestAppContext) {
        let view = a_reader_of(3, a_window_of(1200., 400.), cx);
        let upright = view.update(cx, |view, _| view.zoom_for(Fit::Page).expect("a fit"));
        let on_its_side = view.update(cx, |view, _| {
            view.quarter_turns = 1;
            view.zoom_for(Fit::Page).expect("a fit")
        });

        assert!(
            on_its_side > upright,
            "a page on its side is shorter, so more of it fits: {on_its_side} against {upright}"
        );
    }

    #[gpui::test]
    async fn a_page_number_is_read_as_the_reader_counts_them(cx: &mut TestAppContext) {
        let view = a_reader_of(5, a_window_of(900., 600.), cx);
        // Where each page landed, so asking for one can be told from being
        // ignored: the first page is at the top of the window, the rest below it.
        view.update(cx, |view, _| {
            for (index, cell) in view.page_bounds.iter().enumerate() {
                cell.set(Bounds {
                    origin: point(px(0.), px(index as f32 * 800.)),
                    size: size(px(600.), px(800.)),
                });
            }
        });

        let where_typing_lands = |typed: &str, cx: &mut TestAppContext| {
            view.update(cx, |view, cx| {
                for (index, cell) in view.page_bounds.iter().enumerate() {
                    cell.set(Bounds {
                        origin: point(px(0.), px(index as f32 * 800.)),
                        size: size(px(600.), px(800.)),
                    });
                }
                view.viewport.set(a_window_of(900., 600.));
                view.scroll.set_offset(point(px(0.), px(0.)));
                view.go_to_typed_page(typed, cx);
                view.scroll.offset().y
            })
        };

        for (typed, page) in [("3", 2.), ("1", 0.), ("5", 4.)] {
            let landed = where_typing_lands(typed, cx);
            assert_eq!(
                landed,
                px(-page * 800.),
                "typing {typed} should have gone to page {page}"
            );
        }

        for nonsense in ["0", "6", "", "  ", "twelve", "-2"] {
            assert_eq!(
                where_typing_lands(nonsense, cx),
                px(0.),
                "typing {nonsense:?} is not a page and should have moved nothing"
            );
        }
    }

    #[gpui::test]
    async fn stepping_through_what_was_found_comes_back_round(cx: &mut TestAppContext) {
        let view = a_reader_of(9, a_window_of(900., 600.), cx);
        view.update(cx, |view, _| {
            view.found = [1, 4, 7]
                .into_iter()
                .map(|page| Found {
                    page,
                    bottom: 700.,
                    left: 100.,
                    top: 712.,
                    right: 160.,
                })
                .collect();
            view.searched_for = Some("something".into());
        });

        let seen: Vec<usize> = (0..4)
            .map(|_| {
                view.update(cx, |view, cx| {
                    view.step_match(1, cx);
                    view.at_match
                })
            })
            .collect();
        assert_eq!(
            seen,
            vec![1, 2, 0, 1],
            "the next match after the last is the first again"
        );

        let backwards = view.update(cx, |view, cx| {
            view.at_match = 0;
            view.step_match(-1, cx);
            view.at_match
        });
        assert_eq!(
            backwards, 2,
            "the match before the first is the last, not a wrap around zero"
        );
    }

    #[gpui::test]
    async fn the_pages_scrolled_to_are_the_pages_drawn(cx: &mut TestAppContext) {
        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        let pages = 20;
        view.update(cx, |view, _| {
            view.pages = vec![None; pages];
            view.page_sizes = (0..pages).map(|_| a4()).collect();
            view.page_bounds = (0..pages)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(1000.), px(600.)));
        visual.update(|window, cx| window.draw(cx).clear());

        // The window measures itself, not the document: a reader that thought the
        // window was as tall as the whole document would call every page visible
        // on the first frame and then never draw the ones scrolled to.
        let measured = view.read_with(&mut visual, |view, _| view.viewport.get().size.height);
        assert!(
            measured <= px(601.),
            "the reader measured its window as {measured:?} in a 600px window"
        );

        let at_the_top = view.read_with(&mut visual, |view, _| view.pages_worth_rendering());
        assert!(
            at_the_top.start == 0 && at_the_top.end < pages,
            "at the top of a {pages} page document the reader wanted {at_the_top:?}"
        );

        // As far down as the document goes, the way a wheel or a dragged thumb
        // would leave it.
        let last_page = view.read_with(&mut visual, |view, _| view.page_bounds[pages - 1].get());
        view.update(&mut visual, |view, _| {
            let already = view.scroll.offset();
            let to_the_bottom = last_page.origin.y - view.viewport.get().origin.y;
            view.scroll
                .set_offset(point(already.x, already.y - to_the_bottom));
        });
        visual.update(|window, cx| window.draw(cx).clear());

        let at_the_bottom = view.read_with(&mut visual, |view, _| view.pages_worth_rendering());
        assert!(
            at_the_bottom.contains(&(pages - 1)),
            "scrolled to the last page of {pages}, the reader wanted {at_the_bottom:?}"
        );
        assert!(
            at_the_bottom.start > 0,
            "the reader is still drawing the first pages while looking at the last: \
             it wanted {at_the_bottom:?}"
        );
    }

    #[gpui::test]
    async fn a_drag_across_a_page_selects_that_part_of_it(cx: &mut TestAppContext) {
        use gpui::Modifiers;

        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        view.update(cx, |view, _| {
            view.pages = vec![None; 3];
            view.page_sizes = (0..3).map(|_| a4()).collect();
            view.page_bounds = (0..3)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(1000.), px(700.)));
        // Settled first: the reader is still finding out that there is no engine
        // in a test, and what it has to say about that moves the pages down.
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear());
        visual.update(|window, cx| window.draw(cx).clear());

        // Over the first page as it was actually painted, so the drag is the one
        // a reader would make rather than one in coordinates of our own.
        let page = view.read_with(&mut visual, |view, _| view.page_bounds[0].get());
        assert!(
            page.size.height > px(0.),
            "the page has to have been painted before it can be dragged across"
        );
        let on_the_page = |across: f32, down: f32| {
            point(
                page.origin.x + page.size.width * across,
                page.origin.y + page.size.height * down,
            )
        };

        visual.simulate_event(MouseDownEvent {
            position: on_the_page(0.25, 0.25),
            button: MouseButton::Left,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        visual.update(|window, cx| window.draw(cx).clear());
        visual.simulate_event(MouseMoveEvent {
            position: on_the_page(0.75, 0.6),
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::none(),
        });
        visual.simulate_event(MouseUpEvent {
            position: on_the_page(0.75, 0.6),
            button: MouseButton::Left,
            modifiers: Modifiers::none(),
            click_count: 1,
        });
        visual.update(|window, cx| window.draw(cx).clear());

        let selection = view
            .read_with(&mut visual, |view, _| view.selection.clone())
            .expect("dragging across a page selects part of it");
        assert_eq!(selection.page, 0, "the drag was over the first page");
        let (left, top, right, bottom) = selection.corners();
        for (name, got, wanted) in [
            ("left", left, 0.25),
            ("top", top, 0.25),
            ("right", right, 0.75),
            ("bottom", bottom, 0.6),
        ] {
            assert!(
                (got - wanted).abs() < 0.02,
                "the {name} of the selection came out at {got}, not {wanted}: \
                 what is drawn would not be over what was dragged across"
            );
        }
        assert!(
            !selection.dragging,
            "letting go ends the drag, or the next move over the page keeps drawing"
        );

        // A press somewhere else takes the mark off, which is what makes it
        // possible to be rid of one at all.
        visual.simulate_event(MouseDownEvent {
            position: point(page.origin.x + page.size.width / 2., page.origin.y - px(8.)),
            button: MouseButton::Left,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        visual.update(|window, cx| window.draw(cx).clear());
        let after = view.read_with(&mut visual, |view, _| view.selection.clone());
        assert!(
            after.is_none(),
            "a press away from any page leaves a selection behind: {after:?}"
        );
    }

    #[gpui::test]
    async fn one_page_at_a_time_shows_one_page_and_moves_between_them(cx: &mut TestAppContext) {
        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        view.update(cx, |view, _| {
            view.pages = vec![None; 6];
            view.page_sizes = (0..6).map(|_| a4()).collect();
            view.page_bounds = (0..6)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(1000.), px(700.)));
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear());
        visual.update(|window, cx| window.draw(cx).clear());

        let painted_pages = |visual: &mut VisualTestContext, view: &gpui::Entity<PdfView>| {
            view.read_with(visual, |view, _| {
                view.page_bounds
                    .iter()
                    .filter(|cell| cell.get().size.height > px(0.))
                    .count()
            })
        };
        assert_eq!(
            painted_pages(&mut visual, &view),
            6,
            "a column shows every page it has"
        );

        view.update(&mut visual, |view, cx| {
            view.show_one_page_at_a_time(true, cx)
        });
        visual.update(|window, cx| window.draw(cx).clear());
        // Only one page is laid out now, so the rest keep the place they last
        // had; what matters is which page is asked for and which is on screen.
        assert_eq!(
            view.read_with(&mut visual, |view, _| view.pages_worth_rendering()),
            0..1,
            "one page at a time draws the page being read and no others"
        );

        view.update(&mut visual, |view, cx| view.step_page(1, cx));
        visual.update(|window, cx| window.draw(cx).clear());
        assert_eq!(
            view.read_with(&mut visual, |view, _| (
                view.page_in_view(),
                view.pages_worth_rendering()
            )),
            (1, 1..2),
            "the next page is the one shown, not a scroll of the same one"
        );

        view.update(&mut visual, |view, cx| view.show_page(5, cx));
        visual.update(|window, cx| window.draw(cx).clear());
        assert_eq!(
            view.read_with(&mut visual, |view, _| view.page_in_view()),
            5,
            "asking for the last page shows the last page"
        );

        // And back to the column, still reading the same page.
        view.update(&mut visual, |view, cx| {
            view.show_one_page_at_a_time(false, cx)
        });
        visual.run_until_parked();
        // One frame lays the column out, the next scrolls to the page that was
        // being read, and a third shows the result.
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear());
        }
        assert_eq!(
            painted_pages(&mut visual, &view),
            6,
            "the column is back with every page in it"
        );
        assert_eq!(
            view.read_with(&mut visual, |view, _| view.page_in_view()),
            5,
            "coming back to the column leaves the reader on the page they were on"
        );
    }

    #[gpui::test]
    async fn a_page_that_will_not_draw_is_given_one_more_go_and_no_more(cx: &mut TestAppContext) {
        // There is no engine in a test, so every page fails to draw at once.
        // That is the case worth pinning: a page put back on the list the moment
        // it fails is asked for again on the very next frame, which draws again,
        // which fails -- a spin that pins a core for as long as the document is
        // open.
        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        view.update(cx, |view, _| {
            view.pages = vec![None; 3];
            view.page_sizes = (0..3).map(|_| a4()).collect();
            view.page_bounds = (0..3)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(900.), px(600.)));
        for _ in 0..6 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear());
        }

        let (still_asked_for, tried_twice) = view.read_with(&mut visual, |view, _| {
            (
                view.asked_for.contains(&0),
                view.given_another_go.contains(&0),
            )
        });
        assert!(
            tried_twice,
            "a page that failed to draw has to be worth one more go"
        );
        assert!(
            still_asked_for,
            "after the second failure the page stays asked for, or the reader \
             draws it again on every frame for as long as it is open"
        );
    }

    /// Every key the reader offers has to reach something. An action named in the
    /// keymap with no handler in the view is a key that quietly does nothing --
    /// which is how `ctrl-alt-p` came to be advertised and ignored.
    #[gpui::test]
    async fn the_controls_sit_in_the_middle_of_the_strip(cx: &mut TestAppContext) {
        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        view.update(cx, |view, _| {
            view.pages = vec![None; 6];
            view.page_sizes = (0..6).map(|_| a4()).collect();
            view.page_bounds = (0..6)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        for width in [px(1600.), px(1200.), px(900.)] {
            visual.simulate_resize(size(width, px(700.)));
            visual.update(|window, cx| window.draw(cx).clear());
            visual.update(|window, cx| window.draw(cx).clear());

            let first = visual
                .debug_bounds("pdf-thumbnails")
                .expect("the first control was painted");
            let last = visual
                .debug_bounds("pdf-more")
                .expect("the last control was painted");
            let room_on_the_left = f32::from(first.origin.x);
            let room_on_the_right = f32::from(width - (last.origin.x + last.size.width));
            assert!(
                (room_on_the_left - room_on_the_right).abs() < 4.,
                "at a width of {width:?} the controls left {room_on_the_left} on the left \
                 and {room_on_the_right} on the right"
            );
        }
    }

    #[gpui::test]
    async fn a_dense_screen_gets_a_page_drawn_at_its_own_size(cx: &mut TestAppContext) {
        let view = a_reader_of(3, a_window_of(1000., 700.), cx);
        let at_one_pixel = view.update(cx, |view, _| {
            view.screen_pixels = 1.;
            view.render_width()
        });
        let at_two_pixels = view.update(cx, |view, _| {
            view.screen_pixels = 2.;
            view.render_width()
        });

        assert_eq!(
            at_two_pixels,
            at_one_pixel * 2,
            "a screen with two pixels to a laid-out one needs the page drawn twice \
             as wide, or it is shown scaled up and reads as a blur"
        );

        // And never so wide that one page is tens of megabytes.
        let at_a_deep_zoom = view.update(cx, |view, _| {
            view.screen_pixels = 3.;
            view.zoom = ZOOM_MAX;
            view.render_width()
        });
        assert!(
            at_a_deep_zoom as f32 <= MOST_PIXELS_ACROSS,
            "a page drew {at_a_deep_zoom} pixels across, past the {MOST_PIXELS_ACROSS} cap"
        );
    }

    #[gpui::test]
    async fn the_pages_far_from_the_reader_are_let_go_of(cx: &mut TestAppContext) {
        let view = a_reader_of(40, a_window_of(1000., 700.), cx);
        // As though every page had been drawn, which is what reading a long
        // document from end to end would leave behind.
        let held_at_first = view.update(cx, |view, _| {
            for page in view.pages.iter_mut() {
                *page = Some(Arc::new(RenderImage::new(smallvec::smallvec![
                    image::Frame::new(image::RgbaImage::new(1, 1))
                ])));
            }
            view.pages.iter().filter(|page| page.is_some()).count()
        });
        assert_eq!(held_at_first, 40);

        let still_held = view.update(cx, |view, _| {
            view.let_go_of_distant_pages(&(20..23));
            view.pages.iter().filter(|page| page.is_some()).count()
        });
        assert!(
            still_held <= 3 + PAGES_KEPT_EITHER_SIDE * 2,
            "a reader at pages 20 to 23 still held {still_held} drawn pages"
        );
        assert!(
            view.read_with(cx, |view, _| view.pages[21].is_some()),
            "the page being read has to be one of the ones kept"
        );
    }

    #[gpui::test]
    async fn a_hand_on_the_page_drags_what_is_shown(cx: &mut TestAppContext) {
        use gpui::Modifiers;

        let view = a_reader_of(5, a_window_of(1000., 700.), cx);
        let moved = view.update(cx, |view, cx| {
            view.scroll.set_offset(point(px(0.), px(-400.)));
            view.panning = Some(point(px(500.), px(500.)));
            view.keep_panning(
                &MouseMoveEvent {
                    position: point(px(500.), px(560.)),
                    pressed_button: Some(MouseButton::Middle),
                    modifiers: Modifiers::none(),
                },
                cx,
            );
            view.scroll.offset().y
        });

        assert_eq!(
            moved,
            px(-340.),
            "dragging the page down by 60 has to bring what is above it into view"
        );
    }

    #[gpui::test]
    async fn every_key_the_keymap_offers_reaches_the_reader(cx: &mut TestAppContext) {
        let keymap = include_str!("../../../assets/keymaps/default-linux.json");
        let mut named: Vec<String> = keymap
            .split("\"pdf::")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .map(|name| format!("pdf::{name}"))
            .collect();
        named.sort();
        named.dedup();
        assert!(
            named.len() > 20,
            "the keymap should offer the reader a good many keys, found {named:?}"
        );

        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            view.update(cx, |view, cx| window.focus(&view.focus, cx));
        });
        visual.update(|window, cx| window.draw(cx).clear());

        let reachable: Vec<String> = visual.update(|window, cx| {
            window
                .available_actions(cx)
                .into_iter()
                .map(|action| action.name().to_string())
                .collect()
        });

        let unreachable: Vec<&String> = named
            .iter()
            .filter(|name| !reachable.contains(name))
            .collect();
        assert!(
            unreachable.is_empty(),
            "these keys are in the keymap but reach nothing: {unreachable:?}"
        );
    }

    #[gpui::test]
    async fn a_right_click_over_a_page_opens_the_menu(cx: &mut TestAppContext) {
        use gpui::Modifiers;

        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        view.update(cx, |view, _| {
            view.pages = vec![None; 2];
            view.page_sizes = (0..2).map(|_| a4()).collect();
            view.page_bounds = (0..2)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(1000.), px(700.)));
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear());
        visual.update(|window, cx| window.draw(cx).clear());

        let page = view.read_with(&mut visual, |view, _| view.page_bounds[0].get());
        // Near the top of the page: a page is taller than the window, so its
        // middle is somewhere below the bottom edge and no click can land there.
        let clicked_at = point(
            page.origin.x + page.size.width / 2.,
            page.origin.y + page.size.height * 0.15,
        );
        visual.simulate_event(MouseDownEvent {
            position: clicked_at,
            button: MouseButton::Right,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        visual.update(|window, cx| window.draw(cx).clear());

        let (opened, at) = view.read_with(&mut visual, |view, _| {
            (
                view.context_menu.is_some(),
                view.context_menu.as_ref().map(|(_, at, _)| *at),
            )
        });
        assert!(opened, "a right click over a page has to offer something");
        assert_eq!(
            at,
            Some(clicked_at),
            "the menu opens where the click was, not where the pointer had been"
        );

        // And the page under the click is known, or the menu's own entries have
        // nothing to work on.
        let under = view.read_with(&mut visual, |view, _| view.page_under(clicked_at));
        assert_eq!(
            under,
            Some(0),
            "the click was in the middle of the first page"
        );
    }

    #[gpui::test]
    async fn the_controls_stay_inside_a_narrow_window(cx: &mut TestAppContext) {
        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        view.update(cx, |view, _| {
            view.pages = vec![None; 4];
            view.page_sizes = (0..4).map(|_| a4()).collect();
            view.page_bounds = (0..4)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);

        for width in [
            px(1400.),
            px(1200.),
            px(900.),
            px(760.),
            px(660.),
            px(560.),
            px(420.),
            px(320.),
        ] {
            visual.simulate_resize(size(width, px(600.)));
            // Twice: the strip is laid out from the room it had last time, so the
            // first frame at a new width is what tells it how much that is.
            visual.update(|window, cx| window.draw(cx).clear());
            visual.update(|window, cx| window.draw(cx).clear());

            let viewport = visual.update(|window, _| window.viewport_size());
            let window_area = Bounds {
                origin: point(px(0.), px(0.)),
                size: viewport,
            };
            // What is always on the strip, whatever the pane's width: without a
            // way to zoom and a way to reach the rest, there is no reader.
            let mut always = vec!["pdf-zoom-in", "pdf-zoom-out", "pdf-more"];
            if width >= px(ROOM_FOR_PAGE_NUMBERS) {
                always.push("pdf-next");
            }
            if width >= px(ROOM_FOR_SIDE_LISTS) {
                always.push("pdf-thumbnails");
            }
            if width >= px(ROOM_FOR_SEARCHING) {
                always.push("pdf-find-next");
            }
            for id in always {
                let control = visual
                    .debug_bounds(id)
                    .unwrap_or_else(|| panic!("{id} was painted at a width of {width:?}"));
                assert!(
                    control.origin.x >= px(0.)
                        && control.origin.x + control.size.width <= window_area.size.width + px(1.),
                    "at a width of {width:?} the control {id} painted {control:?}, \
                     outside the {viewport:?} window"
                );
            }
            // The strip fits by leaving things out, so what it leaves out has to
            // be gone rather than merely pushed past the edge.
            if width < px(ROOM_FOR_SEARCHING) {
                assert!(
                    visual.debug_bounds("pdf-find-next").is_none(),
                    "at a width of {width:?} searching should have moved into the menu"
                );
            }
            if width < px(ROOM_FOR_SIDE_LISTS) {
                assert!(
                    visual.debug_bounds("pdf-thumbnails").is_none(),
                    "at a width of {width:?} the side lists should have gone"
                );
            }
            if width < px(ROOM_FOR_PAGE_NUMBERS) {
                assert!(
                    visual.debug_bounds("pdf-next").is_none(),
                    "at a width of {width:?} moving between pages should be in the menu"
                );
            }
        }
    }

    fn selection_over(from: (f32, f32), to: (f32, f32)) -> Selection {
        Selection {
            page: 0,
            from: point(from.0, from.1),
            to: point(to.0, to.1),
            dragging: false,
        }
    }

    /// Three words on the first line, three on the second, laid out the way a
    /// document does it: page points counting up from the bottom.
    fn two_lines_of_text() -> Vec<pdf_engine::PageChar> {
        let mut characters = Vec::new();
        for (line, words) in [(800., "one two"), (760., "three four")] {
            for (at, character) in words.chars().enumerate() {
                let left = 100. + at as f32 * 10.;
                characters.push(pdf_engine::PageChar {
                    character,
                    bottom: line,
                    left,
                    top: line + 12.,
                    right: left + 9.,
                });
            }
        }
        characters
    }

    /// Where the middle of the character at `index` is, as a fraction of the page
    /// from its top left -- which is what a pointer over it would give.
    fn over_character(characters: &[pdf_engine::PageChar], index: usize) -> Point<f32> {
        let character = &characters[index];
        point(
            ((character.left + character.right) / 2.) / a4().width,
            1. - ((character.bottom + character.top) / 2.) / a4().height,
        )
    }

    /// The mark has to be painted over the page. A wrapper placed absolutely with
    /// no corner named lands where it would have gone in the flow -- under the
    /// page's picture, off the bottom of it -- and then the selection is correct in
    /// every way except that nobody can see it.
    #[gpui::test]
    async fn the_mark_over_the_text_is_painted_on_the_page(cx: &mut TestAppContext) {
        a_working_editor(cx);
        let window = cx.add_window(|window, cx| {
            PdfView::open_path(PathBuf::from("/nowhere/document.pdf"), window, cx)
        });
        let view = window.root(cx).expect("the reader was built");
        let characters = two_lines_of_text();
        view.update(cx, |view, _| {
            view.pages = vec![None; 2];
            view.page_sizes = (0..2).map(|_| a4()).collect();
            view.page_bounds = (0..2)
                .map(|_| Rc::new(Cell::new(Bounds::default())))
                .collect();
            view.chars.insert(0, Rc::new(two_lines_of_text()));
            view.selection = Some(Selection {
                page: 0,
                from: over_character(&characters, 0),
                to: over_character(&characters, characters.len() - 1),
                dragging: false,
            });
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(1000.), px(900.)));
        visual.run_until_parked();
        for _ in 0..2 {
            visual.update(|window, cx| window.draw(cx).clear());
        }

        let page = view.read_with(&mut visual, |view, _| view.page_bounds[0].get());
        let mark = visual
            .debug_bounds("pdf-selection")
            .expect("the mark was painted");

        assert!(
            page.size.height > px(0.),
            "the page has to have been painted for this to mean anything"
        );
        assert!(
            (f32::from(mark.origin.y) - f32::from(page.origin.y)).abs() < 1.
                && (f32::from(mark.origin.x) - f32::from(page.origin.x)).abs() < 1.,
            "the mark was painted at {:?} while the page is at {:?}",
            mark.origin,
            page.origin
        );
    }

    #[gpui::test]
    async fn a_drag_over_words_marks_the_words_it_covers(cx: &mut TestAppContext) {
        let characters = two_lines_of_text();
        let view = a_reader_of(1, a_window_of(1000., 700.), cx);
        view.update(cx, |view, _| {
            view.chars.insert(0, Rc::new(two_lines_of_text()));
            view.selection = Some(Selection {
                page: 0,
                from: over_character(&characters, 0),
                to: over_character(&characters, 2),
                dragging: false,
            });
        });

        let (page, covered) = view
            .read_with(cx, |view, _| view.selected_characters())
            .expect("a drag over text covers characters");
        assert_eq!(page, 0);
        assert_eq!(covered, 0..3, "from the first character to the third");

        let copied = view.update(cx, |view, cx| {
            view.read_the_selected_text(cx);
            view.selected_text.clone()
        });
        assert_eq!(
            copied.map(|text| text.to_string()),
            Some("one".to_string()),
            "what is copied has to be the characters the drag went over"
        );
    }

    #[gpui::test]
    async fn a_drag_across_lines_is_marked_a_line_at_a_time(cx: &mut TestAppContext) {
        let characters = two_lines_of_text();
        let view = a_reader_of(1, a_window_of(1000., 700.), cx);
        view.update(cx, |view, _| {
            view.chars.insert(0, Rc::new(two_lines_of_text()));
            view.selection = Some(Selection {
                page: 0,
                from: over_character(&characters, 1),
                to: over_character(&characters, characters.len() - 2),
                dragging: false,
            });
        });

        let (_, covered) = view
            .read_with(cx, |view, _| view.selected_characters())
            .expect("the drag covers characters");
        let bands = bands_over(&characters, covered);
        assert_eq!(
            bands.len(),
            2,
            "two lines are two bands, not one box around both: got {bands:?}"
        );
        assert!(
            bands[0].0 > bands[1].0,
            "the first band is the higher line: {bands:?}"
        );
    }

    #[gpui::test]
    async fn which_way_the_pointer_travelled_makes_no_difference(cx: &mut TestAppContext) {
        let characters = two_lines_of_text();
        let view = a_reader_of(1, a_window_of(1000., 700.), cx);
        view.update(cx, |view, _| {
            view.chars.insert(0, Rc::new(two_lines_of_text()));
        });

        let covered_both_ways: Vec<_> = [(1, 9), (9, 1)]
            .into_iter()
            .map(|(from, to)| {
                view.update(cx, |view, _| {
                    view.selection = Some(Selection {
                        page: 0,
                        from: over_character(&characters, from),
                        to: over_character(&characters, to),
                        dragging: false,
                    });
                });
                view.read_with(cx, |view, _| view.selected_characters())
                    .expect("the drag covers characters")
                    .1
            })
            .collect();

        assert_eq!(
            covered_both_ways[0], covered_both_ways[1],
            "dragging up must mark what dragging down marks"
        );
    }

    #[test]
    fn a_turned_page_maps_a_point_back_to_where_the_document_holds_it() {
        for quarter_turns in 0..4 {
            for at in [
                point(0., 0.),
                point(1., 0.),
                point(0.25, 0.8),
                point(0.9, 0.1),
            ] {
                let there_and_back = as_it_is_painted(
                    as_the_document_lays_it_out(at, quarter_turns),
                    quarter_turns,
                );
                assert!(
                    (there_and_back.x - at.x).abs() < 0.001
                        && (there_and_back.y - at.y).abs() < 0.001,
                    "at {quarter_turns} quarter turns {at:?} came back as {there_and_back:?}"
                );
            }
        }
        // And a turn really does move the point, or the mapping is doing nothing.
        let upright = as_the_document_lays_it_out(point(0.1, 0.2), 0);
        let turned = as_the_document_lays_it_out(point(0.1, 0.2), 1);
        assert_ne!((upright.x, upright.y), (turned.x, turned.y));
    }

    #[test]
    fn a_drag_of_a_few_pixels_is_not_a_selection() {
        assert!(
            !selection_over((0.5, 0.5), (0.5005, 0.5005)).is_worth_reading(),
            "a click that wandered must not read whatever is under the pointer"
        );
        assert!(selection_over((0.2, 0.2), (0.6, 0.4)).is_worth_reading());
    }
}
