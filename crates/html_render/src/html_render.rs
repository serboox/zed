//! Renders HTML the way a browser would, by embedding Servo and taking a
//! picture of the result.
//!
//! Zed's own HTML preview turns HTML into Markdown and renders that, which is
//! readable but not what the page looks like: no flexbox, no grid, no author
//! stylesheet. This crate hands the page to Servo instead and returns the
//! rasterised result as a [`gpui::RenderImage`], the same way mermaid diagrams
//! are already shown.
//!
//! Servo's handles are `Rc`-based and its event loop has to be pumped by hand,
//! so the engine lives on the foreground thread as a [`gpui::Global`] and is
//! driven from a foreground task.

#[cfg(feature = "servo")]
mod engine {
    use std::cell::{Cell, RefCell};
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use anyhow::{Context as _, Result, anyhow};
    use dpi::PhysicalSize;
    use gpui::{App, Global, Pixels, RenderImage, SharedString, Size, Task};
    use servo::{
        EventLoopWaker, LoadStatus, RenderingContext, RgbaImage, Servo, ServoBuilder,
        SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
    };
    use smallvec::SmallVec;

    /// How long a page is given to load and to hand back a picture. A preview that
    /// hangs must not hang the editor with it.
    const RENDER_TIMEOUT: Duration = Duration::from_secs(20);
    /// How often Servo's own event loop is pumped while a render is in flight.
    const SPIN_INTERVAL: Duration = Duration::from_millis(4);

    struct GlobalHtmlEngine(Rc<HtmlEngine>);

    impl Global for GlobalHtmlEngine {}

    /// The embedded browser engine. One per process: Servo keeps global state and
    /// starts its own thread pool, so a second instance is neither cheap nor safe.
    pub struct HtmlEngine {
        servo: Servo,
        rendering_context: Rc<SoftwareRenderingContext>,
        /// The viewport the rendering context was built for. Servo cannot resize a
        /// software surface after the fact, so a bigger request rebuilds nothing --
        /// the page is laid out at this width and the picture is scaled to fit.
        viewport: PhysicalSize<u32>,
    }

    impl HtmlEngine {
        /// The engine for this process, started on first use. Returns an error when
        /// the platform cannot give Servo a rendering surface, so a preview can fall
        /// back to the Markdown rendering rather than take the editor down.
        pub fn global(viewport: PhysicalSize<u32>, cx: &mut App) -> Result<Rc<Self>> {
            if let Some(engine) = cx.try_global::<GlobalHtmlEngine>() {
                return Ok(engine.0.clone());
            }
            let engine = Rc::new(Self::new(viewport)?);
            cx.set_global(GlobalHtmlEngine(engine.clone()));
            Ok(engine)
        }

        fn new(viewport: PhysicalSize<u32>) -> Result<Self> {
            // Surfman panics rather than reporting when there is no EGL to talk to,
            // which is the case on a machine with no GPU stack at all. A preview is
            // not worth a crash, so the panic is caught and reported as an error.
            let rendering_context =
                std::panic::catch_unwind(|| SoftwareRenderingContext::new(viewport))
                    .map_err(|_| {
                        anyhow!("this platform has no rendering surface for HTML previews")
                    })?
                    .context("building a software rendering context for HTML previews")?;
            let rendering_context = Rc::new(rendering_context);
            rendering_context
                .make_current()
                .context("making the HTML preview rendering context current")?;

            let servo = ServoBuilder::default()
                .event_loop_waker(Box::new(Waker(Arc::new(AtomicBool::new(false)))))
                .build();

            Ok(Self {
                servo,
                rendering_context,
                viewport,
            })
        }

        /// The size pages are laid out at.
        pub fn viewport(&self) -> PhysicalSize<u32> {
            self.viewport
        }

        /// Lays `html` out and returns a picture of it. `base_directory` is where
        /// relative links -- stylesheets, images -- are resolved from.
        pub fn render(
            html: SharedString,
            base_directory: Option<&Path>,
            viewport: Size<Pixels>,
            cx: &mut App,
        ) -> Task<Result<Arc<RenderImage>>> {
            let viewport = PhysicalSize {
                width: viewport.width.0.max(1.) as u32,
                height: viewport.height.0.max(1.) as u32,
            };
            let engine = match Self::global(viewport, cx) {
                Ok(engine) => engine,
                Err(error) => return Task::ready(Err(error)),
            };
            let page = match PageFile::write(&html, base_directory) {
                Ok(page) => page,
                Err(error) => return Task::ready(Err(error)),
            };

            cx.spawn(async move |cx| {
                let delegate = Rc::new(Delegate::default());
                let webview = WebViewBuilder::new(&engine.servo, engine.rendering_context.clone())
                    .url(page.url()?)
                    .delegate(delegate.clone())
                    .build();
                webview.focus();

                let deadline = spin_until(&engine, cx, || delegate.loaded.get()).await;
                if !deadline {
                    return Err(anyhow!("the page did not finish loading in time"));
                }

                let shot: Rc<RefCell<Option<RgbaImage>>> = Rc::new(RefCell::new(None));
                let taken = Rc::new(Cell::new(false));
                webview.take_screenshot(None, {
                    let shot = shot.clone();
                    let taken = taken.clone();
                    move |result| {
                        match result {
                            Ok(image) => *shot.borrow_mut() = Some(image),
                            Err(error) => log::warn!("HTML preview screenshot failed: {error:?}"),
                        }
                        taken.set(true);
                    }
                });
                if !spin_until(&engine, cx, || taken.get()).await {
                    return Err(anyhow!("the page was not painted in time"));
                }

                let captured = shot.borrow_mut().take();
                let image = captured.context("the engine returned no picture of the page")?;
                Ok(Arc::new(render_image(image)))
            })
        }
    }

    /// Pumps Servo's event loop on the foreground thread until `ready` says so, or
    /// until the deadline passes. Returns whether `ready` was satisfied.
    async fn spin_until(
        engine: &Rc<HtmlEngine>,
        cx: &mut gpui::AsyncApp,
        ready: impl Fn() -> bool,
    ) -> bool {
        let started = std::time::Instant::now();
        while !ready() {
            if started.elapsed() > RENDER_TIMEOUT {
                return false;
            }
            engine.servo.spin_event_loop();
            cx.background_executor().timer(SPIN_INTERVAL).await;
        }
        true
    }

    /// gpui wants premultiplied BGRA frames; Servo hands out RGBA.
    fn render_image(mut image: RgbaImage) -> RenderImage {
        for pixel in image.pixels_mut() {
            pixel.0.swap(0, 2);
        }
        RenderImage::new(SmallVec::from_elem(image::Frame::new(image), 1))
    }

    /// The page, on disk, for Servo to load over `file://`. Servo loads documents by
    /// URL, and a file next to a `<base href>` is what makes the page's own relative
    /// stylesheets and images resolve the way they do in a browser.
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

    #[derive(Default)]
    struct Delegate {
        loaded: Cell<bool>,
    }

    impl WebViewDelegate for Delegate {
        fn notify_new_frame_ready(&self, webview: WebView) {
            webview.paint();
        }

        fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
            if status == LoadStatus::Complete {
                self.loaded.set(true);
            }
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
pub use engine::HtmlEngine;

/// Without the `servo` feature there is no engine in this build: an HTML preview
/// falls back to the Markdown rendering, which is what it did before.
#[cfg(not(feature = "servo"))]
pub struct HtmlEngine;

#[cfg(not(feature = "servo"))]
impl HtmlEngine {
    pub fn render(
        _html: gpui::SharedString,
        _base_directory: Option<&std::path::Path>,
        _viewport: gpui::Size<gpui::Pixels>,
        _cx: &mut gpui::App,
    ) -> gpui::Task<anyhow::Result<std::sync::Arc<gpui::RenderImage>>> {
        gpui::Task::ready(Err(anyhow::anyhow!(
            "this build has no HTML engine: rebuild with --features servo"
        )))
    }
}
