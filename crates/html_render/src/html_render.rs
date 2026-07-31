//! An embedded browser: pages are laid out, scripted and painted by Servo, and
//! the result is composited into the editor frame by frame.
//!
//! Zed's HTML preview turned a page into Markdown and rendered that, which could
//! never show an author's own stylesheet, let alone run a script. Here the page
//! is a real document in a real engine: CSS and JavaScript run, text can be
//! selected, links can be followed, and the page scrolls itself.
//!
//! Servo's handles are `Rc`-based and its event loop has to be pumped by hand,
//! so everything here lives on the foreground thread.

#[cfg(feature = "servo")]
use primeorder as _;

#[cfg(feature = "servo")]
mod engine {
    use std::cell::{Cell, RefCell};
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::{Context as _, Result, anyhow};
    use dpi::PhysicalSize;
    use gpui::{App, Global, Pixels, Point, RenderImage, SharedString, Size};
    use servo::{
        DeviceIntRect, DevicePoint, EventLoopWaker, InputEvent, LoadStatus, MouseButton,
        MouseButtonAction, MouseButtonEvent, MouseMoveEvent, RenderingContext, RgbaImage, Servo,
        ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
        WebViewPoint, WheelDelta, WheelEvent, WheelMode,
    };
    use smallvec::SmallVec;

    /// The largest page surface kept in memory, in device pixels.
    const MAX_SURFACE: u32 = 8_000;

    struct GlobalHtmlEngine(Rc<HtmlEngine>);

    impl Global for GlobalHtmlEngine {}

    /// The engine itself. One per process: Servo keeps global state and starts
    /// its own thread pool, so a second instance is neither cheap nor safe.
    pub struct HtmlEngine {
        servo: Servo,
    }

    impl HtmlEngine {
        /// The engine for this process, started on first use.
        pub fn global(cx: &mut App) -> Rc<Self> {
            if let Some(engine) = cx.try_global::<GlobalHtmlEngine>() {
                return engine.0.clone();
            }
            let engine = Rc::new(Self {
                servo: ServoBuilder::default()
                    .event_loop_waker(Box::new(Waker(Arc::new(AtomicBool::new(false)))))
                    .build(),
            });
            cx.set_global(GlobalHtmlEngine(engine.clone()));
            engine
        }

        /// Lets the engine work: layout, script and painting all happen here, so
        /// this has to be called regularly for a page to stay alive.
        pub fn spin(&self) {
            self.servo.spin_event_loop();
        }
    }

    /// One live page. Holds its own surface, so two previews never draw over one
    /// another, and hands out the latest frame the engine painted.
    pub struct HtmlPage {
        engine: Rc<HtmlEngine>,
        rendering_context: Rc<SoftwareRenderingContext>,
        webview: WebView,
        delegate: Rc<PageDelegate>,
        size: PhysicalSize<u32>,
        /// Device pixels per editor pixel. The surface is painted at the
        /// display's own resolution, so the page is as sharp as the rest of the
        /// window, and every position given in editor pixels is scaled by this
        /// before the engine sees it.
        scale: f32,
        /// The view the picture is drawn across, in editor pixels.
        viewport: Size<Pixels>,
        frame: Option<Arc<RenderImage>>,
        /// The document on disk. Servo loads by URL, and the file has to outlive
        /// the load.
        document: PageFile,
    }

    impl HtmlPage {
        /// Opens `html` as a page of `size`. `base_directory` is where the page's
        /// own relative links -- stylesheets, scripts, images -- resolve from.
        pub fn open(
            html: SharedString,
            base_directory: Option<&Path>,
            size: Size<Pixels>,
            scale: f32,
            cx: &mut App,
        ) -> Result<Self> {
            let engine = HtmlEngine::global(cx);
            let scale = usable_scale(scale);
            let viewport = size;
            let size = surface_size(size, scale);
            // Surfman panics rather than reporting when there is no EGL to talk
            // to. A preview is not worth a crash, so the panic is caught.
            let rendering_context =
                std::panic::catch_unwind(|| SoftwareRenderingContext::new(size))
                    .map_err(|_| anyhow!("this platform has no rendering surface for HTML"))?
                    .map_err(|error| anyhow!("no rendering surface for HTML: {error:?}"))?;
            let rendering_context = Rc::new(rendering_context);
            rendering_context
                .make_current()
                .map_err(|error| anyhow!("the HTML surface refused to bind: {error:?}"))?;

            let document = PageFile::write(&html, base_directory)?;
            let delegate = Rc::new(PageDelegate::default());
            let webview = WebViewBuilder::new(&engine.servo, rendering_context.clone())
                .url(document.url()?)
                .hidpi_scale_factor(euclid::Scale::new(scale))
                .delegate(delegate.clone())
                .build();
            webview.focus();
            webview.resize(size);

            Ok(Self {
                engine,
                rendering_context,
                webview,
                delegate,
                size,
                scale,
                viewport,
                frame: None,
                document,
            })
        }

        /// Points the page at a fresh document, keeping the same engine and
        /// surface: an edit reloads rather than starting a new browser.
        pub fn reload(&mut self, html: SharedString, base_directory: Option<&Path>) -> Result<()> {
            let document = PageFile::write(&html, base_directory)?;
            self.webview.load(document.url()?);
            self.document = document;
            Ok(())
        }

        pub fn resize(&mut self, size: Size<Pixels>, scale: f32) {
            let scale = usable_scale(scale);
            self.viewport = size;
            let size = surface_size(size, scale);
            if size == self.size && scale == self.scale {
                return;
            }
            self.bind();
            if scale != self.scale {
                self.scale = scale;
                self.webview
                    .set_hidpi_scale_factor(euclid::Scale::new(scale));
            }
            self.size = size;
            // Only the webview is told: it resizes the surface itself, and a
            // surface already set to the new size makes its own resize a no-op,
            // which leaves the compositor drawing at the old one.
            self.webview.resize(size);
        }

        /// Lets the engine run and picks up the newest frame it painted. Returns
        /// whether there is something new to show.
        pub fn pump(&mut self) -> bool {
            self.bind();
            self.engine.spin();
            if !self.delegate.painted.replace(false) {
                return false;
            }
            let rect = DeviceIntRect::from_size(euclid::Size2D::new(
                self.size.width as i32,
                self.size.height as i32,
            ));
            match self.rendering_context.read_to_image(rect) {
                Some(image) => {
                    self.frame = Some(Arc::new(render_image(image)));
                    true
                }
                None => false,
            }
        }

        pub fn frame(&self) -> Option<Arc<RenderImage>> {
            self.frame.clone()
        }

        /// Makes this page's surface the one the engine draws into and reads
        /// back from. With a second preview open the current surface is
        /// whichever page bound it last, and painting through another page's
        /// surface gives that page's pixels, or none at all.
        fn bind(&self) {
            if let Err(error) = self.rendering_context.make_current() {
                log::warn!("the HTML surface could not be bound: {error:?}");
            }
        }

        pub fn is_loading(&self) -> bool {
            self.delegate.load_status.get() != LoadStatus::Complete
        }

        /// A link the page asked to open elsewhere -- a target of `_blank`, or a
        /// middle click. The caller decides what "elsewhere" means.
        pub fn take_link_for_new_tab(&self) -> Option<url::Url> {
            self.delegate.link_for_new_tab.borrow_mut().take()
        }

        pub fn mouse_moved(&self, point: Point<Pixels>) {
            self.webview
                .notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                    self.device(point),
                )));
        }

        pub fn mouse_down(&self, point: Point<Pixels>, button: MouseButton) {
            self.webview
                .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                    MouseButtonAction::Down,
                    button,
                    self.device(point),
                )));
        }

        pub fn mouse_up(&self, point: Point<Pixels>, button: MouseButton) {
            self.webview
                .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                    MouseButtonAction::Up,
                    button,
                    self.device(point),
                )));
        }

        /// Both halves of a wheel turn: the event a script can see, and the
        /// scroll the compositor performs.
        pub fn scrolled(&self, point: Point<Pixels>, delta: Point<Pixels>) {
            let (horizontal, vertical) = self.painted_scale();
            let (x, y) = (
                (f32::from(delta.x) * horizontal) as f64,
                (f32::from(delta.y) * vertical) as f64,
            );
            self.webview
                .notify_input_event(InputEvent::Wheel(WheelEvent::new(
                    WheelDelta {
                        x,
                        y,
                        z: 0.,
                        mode: WheelMode::DeltaPixel,
                    },
                    self.device(point),
                )));
            self.webview.notify_scroll_event(
                servo::Scroll::Delta(servo::WebViewVector::Device(servo::DeviceVector2D::new(
                    x as f32, y as f32,
                ))),
                self.device(point),
            );
        }

        pub fn key(&self, event: keyboard_types::KeyboardEvent) {
            self.webview
                .notify_input_event(InputEvent::Keyboard(servo::KeyboardEvent::new(event)));
        }

        /// Device pixels per editor pixel, taken from the sizes themselves. It
        /// is the display's scale, except where the surface hit its ceiling:
        /// then the picture is stretched over the view and a click has to be
        /// measured against the surface the page really has.
        fn painted_scale(&self) -> (f32, f32) {
            let ratio = |surface: u32, viewport: Pixels| {
                let viewport = f32::from(viewport);
                if viewport > 0. {
                    surface as f32 / viewport
                } else {
                    self.scale
                }
            };
            (
                ratio(self.size.width, self.viewport.width),
                ratio(self.size.height, self.viewport.height),
            )
        }

        /// Where a position in editor pixels lands on the surface.
        fn device(&self, point: Point<Pixels>) -> WebViewPoint {
            let (horizontal, vertical) = self.painted_scale();
            WebViewPoint::Device(DevicePoint::new(
                f32::from(point.x).max(0.) * horizontal,
                f32::from(point.y).max(0.) * vertical,
            ))
        }
    }

    /// A display scale that cannot turn a page into a surface of no pixels or
    /// of far too many.
    fn usable_scale(scale: f32) -> f32 {
        if scale.is_finite() { scale.clamp(0.5, 4.) } else { 1. }
    }

    fn surface_size(size: Size<Pixels>, scale: f32) -> PhysicalSize<u32> {
        let device = |length: Pixels| {
            ((f32::from(length) * scale).max(1.) as u32).min(MAX_SURFACE)
        };
        PhysicalSize {
            width: device(size.width),
            height: device(size.height),
        }
    }

    /// gpui wants premultiplied BGRA frames; Servo hands out RGBA.
    fn render_image(mut image: RgbaImage) -> RenderImage {
        for pixel in image.pixels_mut() {
            pixel.0.swap(0, 2);
        }
        RenderImage::new(SmallVec::from_elem(image::Frame::new(image), 1))
    }

    /// The document, on disk, for Servo to load over `file://`. A `<base href>`
    /// is what makes the page's own relative links resolve the way they do in a
    /// browser.
    struct PageFile {
        _directory: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl PageFile {
        fn write(html: &str, base_directory: Option<&Path>) -> Result<Self> {
            let directory = tempfile::tempdir().context("creating a directory for the page")?;
            let path = directory.path().join("page.html");
            let document =
                match base_directory.and_then(|base| url::Url::from_directory_path(base).ok()) {
                    Some(base) => format!("<base href=\"{base}\">\n{html}"),
                    None => html.to_string(),
                };
            std::fs::write(&path, document).context("writing the page for the engine to load")?;
            Ok(Self {
                _directory: directory,
                path,
            })
        }

        fn url(&self) -> Result<url::Url> {
            url::Url::from_file_path(&self.path)
                .map_err(|_| anyhow!("the page's own path is not a valid file url"))
        }
    }

    struct PageDelegate {
        painted: Cell<bool>,
        load_status: Cell<LoadStatus>,
        link_for_new_tab: RefCell<Option<url::Url>>,
    }

    impl Default for PageDelegate {
        fn default() -> Self {
            Self {
                painted: Cell::new(false),
                load_status: Cell::new(LoadStatus::Started),
                link_for_new_tab: RefCell::new(None),
            }
        }
    }

    impl WebViewDelegate for PageDelegate {
        fn notify_new_frame_ready(&self, webview: WebView) {
            webview.paint();
            self.painted.set(true);
        }

        fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
            self.load_status.set(status);
        }

        /// A link the reader followed. The page navigates as a browser does, and
        /// the address is kept so the editor can offer to open it in a tab of
        /// its own.
        fn request_navigation(
            &self,
            _webview: WebView,
            navigation_request: servo::NavigationRequest,
        ) {
            *self.link_for_new_tab.borrow_mut() = Some(navigation_request.url.clone());
            navigation_request.allow();
        }
    }

    #[derive(Clone)]
    struct Waker(Arc<AtomicBool>);

    impl EventLoopWaker for Waker {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }

        fn wake(&self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(feature = "servo")]
pub use engine::{HtmlEngine, HtmlPage};
#[cfg(feature = "servo")]
pub use servo::MouseButton;

/// Without the `servo` feature there is no engine in this build: an HTML preview
/// falls back to the Markdown rendering, which is what it did before.
#[cfg(not(feature = "servo"))]
pub struct HtmlPage;

#[cfg(not(feature = "servo"))]
impl HtmlPage {
    pub fn open(
        _html: gpui::SharedString,
        _base_directory: Option<&std::path::Path>,
        _size: gpui::Size<gpui::Pixels>,
        _scale: f32,
        _cx: &mut gpui::App,
    ) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!(
            "this build has no HTML engine: rebuild with --features servo"
        ))
    }
}
