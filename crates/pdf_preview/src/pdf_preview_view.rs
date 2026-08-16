use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, RenderImage, ScrollHandle,
    SharedString, Task, Window, canvas, div, img, point, px,
};
use pdfium_render::prelude::PdfPageIndex;
use smallvec::SmallVec;
use ui::{WithScrollbar, prelude::*};
use workspace::{Item, ToolbarItemLocation, item::ItemEvent};

use crate::{PdfCopy, PdfZoomIn, PdfZoomOut, PdfZoomReset, pdf_engine, pdf_item::PdfItem};

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
const ZOOM_MIN: f32 = 0.4;
const ZOOM_MAX: f32 = 4.0;

/// How many pages are rendered eagerly. The rest follow as they are scrolled to.
const PAGES_RENDERED_AT_ONCE: usize = 3;

/// A drag shorter than this in either direction is a click that wandered, not a
/// selection, and asking the engine for the text under it would return whatever
/// happens to sit beneath the pointer.
const SMALLEST_SELECTION: f32 = 4.;

/// What the reader is dragging over a page, in the window's own coordinates.
struct Selection {
    page: usize,
    from: Point<Pixels>,
    to: Point<Pixels>,
    /// Whether the pointer is still down. A finished selection stays on screen
    /// so it can be copied.
    dragging: bool,
}

impl Selection {
    fn bounds(&self) -> Bounds<Pixels> {
        let left = self.from.x.min(self.to.x);
        let top = self.from.y.min(self.to.y);
        let right = self.from.x.max(self.to.x);
        let bottom = self.from.y.max(self.to.y);
        Bounds {
            origin: point(left, top),
            size: gpui::size(right - left, bottom - top),
        }
    }

    fn is_worth_reading(&self) -> bool {
        let bounds = self.bounds();
        f32::from(bounds.size.width) >= SMALLEST_SELECTION
            && f32::from(bounds.size.height) >= SMALLEST_SELECTION
    }
}

/// Turns a rectangle drawn on screen into the rectangle a PDF page is laid out
/// in: points, with the origin at the bottom left rather than the top.
///
/// Pure so the arithmetic can be tested without a window: it is the one part of
/// selecting text that has no way of announcing that it is wrong -- text simply
/// comes back from somewhere else on the page.
fn screen_rect_to_page_points(
    selection: Bounds<Pixels>,
    page_on_screen: Bounds<Pixels>,
    page: &pdf_engine::PageSize,
) -> Option<(f32, f32, f32, f32)> {
    let width_on_screen = f32::from(page_on_screen.size.width);
    let height_on_screen = f32::from(page_on_screen.size.height);
    if width_on_screen <= 0. || height_on_screen <= 0. {
        return None;
    }

    let left_fraction =
        (f32::from(selection.origin.x - page_on_screen.origin.x) / width_on_screen).clamp(0., 1.);
    let right_fraction =
        (f32::from(selection.origin.x + selection.size.width - page_on_screen.origin.x)
            / width_on_screen)
            .clamp(0., 1.);
    let top_fraction =
        (f32::from(selection.origin.y - page_on_screen.origin.y) / height_on_screen).clamp(0., 1.);
    let bottom_fraction =
        (f32::from(selection.origin.y + selection.size.height - page_on_screen.origin.y)
            / height_on_screen)
            .clamp(0., 1.);

    let left = left_fraction * page.width;
    let right = right_fraction * page.width;
    // A page counts upwards from its bottom edge, a window downwards from its
    // top, so the two vertical fractions swap ends here.
    let top = (1. - top_fraction) * page.height;
    let bottom = (1. - bottom_fraction) * page.height;
    Some((bottom, left, top, right))
}

pub struct PdfView {
    path: PathBuf,
    title: SharedString,
    page_count: PdfPageIndex,
    /// Rendered pages by index. A page absent from here has not been rendered at
    /// this zoom yet, and shows its place until it has.
    pages: Vec<Option<Arc<RenderImage>>>,
    zoom: f32,
    focus: FocusHandle,
    scroll: ScrollHandle,
    rendering: Option<Task<()>>,
    trouble: Option<SharedString>,
    /// Where each page was last painted, so a position in the window can be told
    /// which page it is on and where on it.
    page_bounds: Vec<Rc<Cell<Bounds<Pixels>>>>,
    /// Each page's own size, in the points it is laid out in.
    page_sizes: Vec<pdf_engine::PageSize>,
    selection: Option<Selection>,
    /// The text under the finished selection, once the engine has read it.
    selected_text: Option<SharedString>,
    reading_text: Option<Task<()>>,
}

impl PdfView {
    pub fn new(item: Entity<PdfItem>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = item.read(cx).abs_path.clone();
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "PDF".to_string())
            .into();

        let mut view = Self {
            path,
            title,
            page_count: 0,
            pages: Vec::new(),
            zoom: 1.,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            rendering: None,
            trouble: None,
            page_bounds: Vec::new(),
            page_sizes: Vec::new(),
            selection: None,
            selected_text: None,
            reading_text: None,
        };
        view.read_the_document(cx);
        view
    }

    /// Counts the pages, then renders the first few. Everything the engine does
    /// happens off the interface thread: opening a document parses it, and a
    /// large one takes long enough to be felt.
    fn read_the_document(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let width = self.render_width();
        self.rendering = Some(cx.spawn(async move |view, cx| {
            let read = cx
                .background_spawn(async move {
                    let engine = pdf_engine::bind()?;
                    let count = pdf_engine::page_count(&engine, &path)?;
                    let sizes = pdf_engine::page_sizes(&engine, &path)?;
                    let mut rendered = Vec::new();
                    for index in 0..count.min(PAGES_RENDERED_AT_ONCE as PdfPageIndex) {
                        rendered.push(pdf_engine::render_page(&engine, &path, index, width)?);
                    }
                    anyhow::Ok((count, sizes, rendered))
                })
                .await;

            view.update(cx, |view, cx| {
                match read {
                    Ok((count, sizes, rendered)) => {
                        view.page_count = count;
                        view.pages = vec![None; count.max(0) as usize];
                        view.page_bounds = (0..count.max(0) as usize)
                            .map(|_| Rc::new(Cell::new(Bounds::default())))
                            .collect();
                        view.page_sizes = sizes;
                        for (index, page) in rendered.into_iter().enumerate() {
                            view.pages[index] = Some(Arc::new(as_render_image(page)));
                        }
                        view.trouble = None;
                    }
                    Err(error) => {
                        log::error!("the PDF did not open: {error:#}");
                        view.trouble = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The width pages are rendered at, which is the base width scaled by the
    /// reader's zoom.
    fn render_width(&self) -> u32 {
        (BASE_PAGE_WIDTH * self.zoom).round().max(1.) as u32
    }

    fn zoom_by(&mut self, step: f32, cx: &mut Context<Self>) {
        let wanted = (self.zoom + step).clamp(ZOOM_MIN, ZOOM_MAX);
        if (wanted - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = wanted;
        // Every page held is at the old width, so they go and are rendered again.
        self.pages = vec![None; self.pages.len()];
        self.read_the_document(cx);
        cx.notify();
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Which page a position in the window is over, if any.
    fn page_under(&self, position: Point<Pixels>) -> Option<usize> {
        self.page_bounds
            .iter()
            .position(|bounds| bounds.get().contains(&position))
    }

    fn start_selecting(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(page) = self.page_under(event.position) else {
            return;
        };
        self.selected_text = None;
        self.selection = Some(Selection {
            page,
            from: event.position,
            to: event.position,
            dragging: true,
        });
        cx.notify();
    }

    fn keep_selecting(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        if !selection.dragging {
            return;
        }
        selection.to = event.position;
        cx.notify();
    }

    fn finish_selecting(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        selection.dragging = false;
        if !selection.is_worth_reading() {
            self.selection = None;
            cx.notify();
            return;
        }
        self.read_the_selected_text(cx);
        cx.notify();
    }

    /// Asks the engine for the text under the finished selection. Reading a page
    /// means opening the document again, so it happens off the interface thread
    /// like every other errand to the engine.
    fn read_the_selected_text(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let Some(page_on_screen) = self.page_bounds.get(selection.page).map(|cell| cell.get())
        else {
            return;
        };
        let Some(page_size) = self.page_sizes.get(selection.page) else {
            return;
        };
        let Some((bottom, left, top, right)) =
            screen_rect_to_page_points(selection.bounds(), page_on_screen, page_size)
        else {
            return;
        };

        let path = self.path.clone();
        let page_index = selection.page as PdfPageIndex;
        self.reading_text = Some(cx.spawn(async move |view, cx| {
            let read = cx
                .background_spawn(async move {
                    let engine = pdf_engine::bind()?;
                    pdf_engine::text_in_rect(&engine, &path, page_index, bottom, left, top, right)
                })
                .await;
            view.update(cx, |view, cx| {
                match read {
                    Ok(text) if !text.trim().is_empty() => {
                        view.selected_text = Some(text.into());
                    }
                    Ok(_) => view.selected_text = None,
                    Err(error) => {
                        log::warn!("the PDF's text could not be read: {error:#}");
                        view.selected_text = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn copy_the_selection(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.selected_text.clone() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    }

    /// The rectangle being dragged, drawn over the page it belongs to.
    fn render_selection(&self) -> Option<AnyElement> {
        let selection = self.selection.as_ref()?;
        let bounds = selection.bounds();
        Some(
            div()
                .absolute()
                .left(bounds.origin.x)
                .top(bounds.origin.y)
                .w(bounds.size.width)
                .h(bounds.size.height)
                .bg(ui::cyberpunk::Accent::Cyan.border().opacity(0.18))
                .border_1()
                .border_color(ui::cyberpunk::Accent::Cyan.border())
                .into_any_element(),
        )
    }

    fn render_page_placeholder(&self, index: usize) -> AnyElement {
        div()
            .w(px(BASE_PAGE_WIDTH * self.zoom))
            .h(px(BASE_PAGE_WIDTH * self.zoom * 1.414))
            .bg(cx_surface())
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
}

/// A page's pixels as the window holds them. They already come back blue first,
/// which is the order `RenderImage` is uploaded in.
fn as_render_image(page: pdf_engine::RenderedPage) -> RenderImage {
    let buffer = image::RgbaImage::from_raw(page.width, page.height, page.pixels)
        .expect("the engine reports the size of the pixels it returned");
    RenderImage::new(SmallVec::from_elem(image::Frame::new(buffer), 1))
}

fn cx_surface() -> gpui::Hsla {
    ui::cyberpunk::surface()
}

impl Focusable for PdfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<()> for PdfView {}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages: Vec<AnyElement> = (0..self.pages.len())
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
                    // Where this page ended up, recorded without taking any room:
                    // a position in the window means nothing until it can be told
                    // which page it is over and where on it.
                    .children(where_it_lands.map(|cell| {
                        canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| ())
                            .absolute()
                            .size_full()
                    }))
                    .into_any_element()
            })
            .collect();

        div()
            .id("pdf-view")
            .key_context("PdfView")
            .track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &PdfZoomIn, _, cx| view.zoom_by(ZOOM_STEP, cx)))
            .on_action(cx.listener(|view, _: &PdfZoomOut, _, cx| view.zoom_by(-ZOOM_STEP, cx)))
            .on_action(cx.listener(|view, _: &PdfZoomReset, _, cx| {
                let back_to_one = 1. - view.zoom;
                view.zoom_by(back_to_one, cx);
            }))
            .on_action(cx.listener(|view, _: &PdfCopy, _, cx| view.copy_the_selection(cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, _, cx| view.start_selecting(event, cx)),
            )
            .on_mouse_move(
                cx.listener(|view, event: &MouseMoveEvent, _, cx| view.keep_selecting(event, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &MouseUpEvent, _, cx| view.finish_selecting(event, cx)),
            )
            .size_full()
            .bg(cx_surface())
            .child(
                div()
                    .id("pdf-pages")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_4()
                    .child(
                        v_flex()
                            .items_center()
                            .gap_4()
                            .children(self.trouble.clone().map(|trouble| {
                                Label::new(trouble).color(Color::Error).into_any_element()
                            }))
                            .children(pages),
                    ),
            )
            .children(self.render_selection())
            .vertical_scrollbar_for(&self.scroll, window, cx)
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

    fn show_toolbar(&self) -> bool {
        true
    }

    fn breadcrumb_location(&self, _cx: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_page_on_screen() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(100.), px(50.)),
            size: gpui::size(px(400.), px(800.)),
        }
    }

    /// A4 in points, near enough: what the engine reports a page's own size to be.
    fn a4() -> pdf_engine::PageSize {
        pdf_engine::PageSize {
            width: 595.,
            height: 842.,
        }
    }

    #[test]
    fn a_selection_over_the_whole_page_covers_the_whole_page() {
        let (bottom, left, top, right) =
            screen_rect_to_page_points(a_page_on_screen(), a_page_on_screen(), &a4())
                .expect("a page with real area converts");

        assert!((left - 0.).abs() < 0.01, "left edge, got {left}");
        assert!((right - 595.).abs() < 0.01, "right edge, got {right}");
        assert!((bottom - 0.).abs() < 0.01, "bottom edge, got {bottom}");
        assert!((top - 842.).abs() < 0.01, "top edge, got {top}");
    }

    // A page counts upwards from its bottom edge and a window downwards from its
    // top, so a rectangle over the *top* of the page has to come back as the
    // *higher* numbers. Getting this backwards returns text from the mirror image
    // of what was selected, which nothing else would report.
    #[test]
    fn the_top_of_the_page_on_screen_is_the_top_of_the_page_in_points() {
        let top_quarter = Bounds {
            origin: point(px(100.), px(50.)),
            size: gpui::size(px(400.), px(200.)),
        };
        let (bottom, _, top, _) =
            screen_rect_to_page_points(top_quarter, a_page_on_screen(), &a4())
                .expect("a page with real area converts");

        assert!(
            (top - 842.).abs() < 0.01,
            "the top stays the top, got {top}"
        );
        assert!(
            (bottom - 631.5).abs() < 0.5,
            "a quarter down the page, got {bottom}"
        );
    }

    #[test]
    fn a_selection_reaching_past_the_page_is_held_to_it() {
        let past_the_edges = Bounds {
            origin: point(px(-500.), px(-500.)),
            size: gpui::size(px(4000.), px(4000.)),
        };
        let (bottom, left, top, right) =
            screen_rect_to_page_points(past_the_edges, a_page_on_screen(), &a4())
                .expect("a page with real area converts");

        assert!(
            left >= 0. && right <= 595.,
            "held to the page: {left}..{right}"
        );
        assert!(
            bottom >= 0. && top <= 842.,
            "held to the page: {bottom}..{top}"
        );
    }

    #[test]
    fn a_page_with_no_area_yet_converts_nothing() {
        let not_painted = Bounds::default();
        assert!(
            screen_rect_to_page_points(a_page_on_screen(), not_painted, &a4()).is_none(),
            "a page that has not been painted has no coordinates to map onto"
        );
    }

    #[test]
    fn a_drag_of_a_few_pixels_is_not_a_selection() {
        let barely_moved = Selection {
            page: 0,
            from: point(px(10.), px(10.)),
            to: point(px(12.), px(11.)),
            dragging: false,
        };
        assert!(
            !barely_moved.is_worth_reading(),
            "a click that wandered must not read whatever is under the pointer"
        );

        let a_real_drag = Selection {
            page: 0,
            from: point(px(10.), px(10.)),
            to: point(px(120.), px(60.)),
            dragging: false,
        };
        assert!(a_real_drag.is_worth_reading());
    }
}
