use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, RenderImage, ScrollHandle,
    SharedString, Task, Window, div, img, px,
};
use pdfium_render::prelude::PdfPageIndex;
use smallvec::SmallVec;
use ui::{WithScrollbar, prelude::*};
use workspace::{Item, ToolbarItemLocation, item::ItemEvent};

use crate::{PdfZoomIn, PdfZoomOut, PdfZoomReset, pdf_engine, pdf_item::PdfItem};

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
                    let mut rendered = Vec::new();
                    for index in 0..count.min(PAGES_RENDERED_AT_ONCE as PdfPageIndex) {
                        rendered.push(pdf_engine::render_page(&engine, &path, index, width)?);
                    }
                    anyhow::Ok((count, rendered))
                })
                .await;

            view.update(cx, |view, cx| {
                match read {
                    Ok((count, rendered)) => {
                        view.page_count = count;
                        view.pages = vec![None; count.max(0) as usize];
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
            .map(|index| match self.pages[index].clone() {
                Some(page) => img(gpui::ImageSource::Render(page))
                    .w(px(BASE_PAGE_WIDTH * self.zoom))
                    .into_any_element(),
                None => self.render_page_placeholder(index),
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
