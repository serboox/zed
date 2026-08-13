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

#[cfg(all(feature = "servo", target_os = "linux"))]
mod dma_buf_export;
#[cfg(feature = "servo")]
mod gpu_surface;
#[cfg(all(feature = "servo", target_os = "linux"))]
mod shared_buffer;
#[cfg(all(feature = "servo", target_os = "windows"))]
mod shared_texture;

#[cfg(feature = "servo")]
mod engine {
    use crate::gpu_surface::GpuSurface;
    use std::cell::{Cell, RefCell};
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Arc;

    use anyhow::{Context as _, Result, anyhow};
    use dpi::PhysicalSize;
    use gpui::{App, Global, Pixels, Point, RenderImage, SharedString, Size};
    use servo::{
        DevicePoint, EventLoopWaker, InputEvent, LoadStatus, MouseButton, MouseButtonAction,
        MouseButtonEvent, MouseMoveEvent, RenderingContext, RgbaImage, Servo, ServoBuilder,
        WebView, WebViewBuilder, WebViewDelegate, WebViewPoint, WheelDelta, WheelEvent, WheelMode,
    };
    use smallvec::SmallVec;

    /// What the engine tells a page it is. Sites read this to decide what to
    /// send, and one they do not recognise gets the treatment reserved for
    /// browsers nobody has heard of: an older layout, a warning, or a refusal.
    /// This is what Firefox's own nightly builds send on this platform.
    const FIREFOX_NIGHTLY: &str = if cfg!(target_os = "windows") {
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:148.0) Gecko/20100101 Firefox/148.0"
    } else if cfg!(target_os = "macos") {
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:148.0) Gecko/20100101 Firefox/148.0"
    } else {
        "Mozilla/5.0 (X11; Linux x86_64; rv:148.0) Gecko/20100101 Firefox/148.0"
    };

    /// The largest page surface kept in memory, in device pixels.
    const MAX_SURFACE: u32 = 8_000;

    /// The locale every page is read in, whatever the machine's own. Language
    /// negotiation follows this, so a site answers in English in every country
    /// rather than in the language of wherever the reader happens to be.
    const READING_LOCALE: &str = "en-US";

    struct GlobalHtmlEngine(Rc<HtmlEngine>);

    impl Global for GlobalHtmlEngine {}

    /// What the engine is set up with. Read once, when it starts: Servo takes
    /// these at build time and there is one engine for the life of the editor,
    /// so a change to them is worth nothing until the editor is opened again.
    #[derive(Clone, Default)]
    pub struct EngineOptions {
        /// Where the engine's debugging server listens, so a browser's own
        /// developer tools can attach to the page. Absent means no server.
        pub devtools_port: Option<u16>,
        /// Somewhere to send requests through, when the reader wants the web to
        /// see them arriving from elsewhere.
        pub proxy: Option<String>,
    }

    struct GlobalEngineOptions(EngineOptions);

    impl Global for GlobalEngineOptions {}

    /// Says how the engine should be set up. Only what is said before the engine
    /// starts counts.
    pub fn set_engine_options(options: EngineOptions, cx: &mut App) {
        cx.set_global(GlobalEngineOptions(options));
    }

    fn engine_options(cx: &App) -> EngineOptions {
        cx.try_global::<GlobalEngineOptions>()
            .map(|global| global.0.clone())
            .unwrap_or_default()
    }

    /// The engine itself. One per process: Servo keeps global state and starts
    /// its own thread pool, so a second instance is neither cheap nor safe.
    pub struct HtmlEngine {
        /// Taken away when the editor is closing. Dropping it is how Servo is
        /// told to stop -- it sends its own threads away and waits for them --
        /// and it has to happen while the surfaces it draws through are still
        /// there, which is why it is done deliberately rather than left to
        /// whenever this happens to be dropped.
        servo: RefCell<Option<Servo>>,
        /// Told whenever the engine has work to do. Servo drives its embedder
        /// this way -- a page at rest never asks for anything -- which is why
        /// nothing here polls it on a timer.
        woken: async_channel::Receiver<()>,
        tell: async_channel::Sender<()>,
    }

    impl HtmlEngine {
        /// The engine for this process, started on first use.
        pub fn global(cx: &mut App) -> Rc<Self> {
            if let Some(engine) = cx.try_global::<GlobalHtmlEngine>() {
                return engine.0.clone();
            }
            // The engine's network thread refuses to start without one, and
            // whether anything else in the editor has chosen one by now is not
            // something a preview should depend on. Already chosen is fine.
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .ok();
            // One is enough: a second pending wake-up says nothing the first
            // one did not, so the sender drops it rather than queueing work.
            let (tell, woken) = async_channel::bounded(1);
            let options = engine_options(cx);
            let engine = Rc::new(Self {
                servo: RefCell::new(Some(
                    ServoBuilder::default()
                        .preferences(preferences(&options))
                        .event_loop_waker(Box::new(Waker(tell.clone())))
                        .build(),
                )),
                woken,
                tell,
            });
            cx.set_global(GlobalHtmlEngine(engine.clone()));
            // Told to stop before the editor takes its windows down. Servo waits
            // for its own threads on the way out, and it cannot do that through
            // surfaces that have already gone: left to the order things happen
            // to be dropped in, the editor never finished closing.
            cx.on_app_quit(|cx| {
                if cx.has_global::<GlobalHtmlEngine>() {
                    let engine = cx.remove_global::<GlobalHtmlEngine>();
                    let started = std::time::Instant::now();
                    log::info!("the HTML engine is being told to stop");
                    engine.0.stop();
                    log::info!("the HTML engine stopped in {:?}", started.elapsed());
                }
                async {}
            })
            .detach();
            engine
        }

        /// Lets the engine work: layout, script and painting all happen here, so
        /// this has to be called regularly for a page to stay alive. A page
        /// pumped after the engine has stopped simply has nothing to do.
        pub fn spin(&self) {
            if let Some(servo) = self.servo.borrow().as_ref() {
                servo.spin_event_loop();
            }
        }

        /// Tells the engine to stop, and waits while it does. Servo's own way of
        /// being told is to be dropped: it sends its threads away and turns its
        /// loop over until they have gone.
        pub fn stop(&self) {
            let servo = self.servo.borrow_mut().take();
            drop(servo);
        }

        /// Waits until the engine has something to do. Whoever drives a page
        /// awaits this instead of asking again and again.
        pub async fn wait_for_work(&self) {
            self.woken.recv().await.ok();
        }

        /// Says there is work without waiting for the engine to say so. Handing
        /// the engine an event or a script is work it has not noticed yet.
        pub fn nudge(&self) {
            self.tell.try_send(()).ok();
        }
    }

    /// How the engine is set up for a preview rather than for a browser.
    ///
    /// Text is drawn with grey edges rather than coloured ones: subpixel
    /// antialiasing costs the graphics card a second pass over every glyph and
    /// only looks right on a display whose pixels are arranged the way it
    /// assumes. Layout is given as many threads as the machine will spare, since
    /// a preview is laid out again on every keystroke in the source.
    fn preferences(options: &EngineOptions) -> servo::Preferences {
        let mut preferences = servo::Preferences::default();
        // The engine keeps several parts of CSS behind switches of its own, all
        // off. A page built on any of them -- and a grid is how most pages are
        // built now -- falls back to laying everything out one block under
        // another against the left edge, which is not what a preview is for.
        preferences.layout_grid_enabled = true;
        preferences.layout_columns_enabled = true;
        preferences.layout_container_queries_enabled = true;
        preferences.layout_writing_mode_enabled = true;
        preferences.layout_variable_fonts_enabled = true;
        preferences.layout_css_attr_enabled = true;
        // Pages are asked for in American English, whatever the machine's own
        // locale: this sets both what `navigator.language` says and the
        // Accept-Language every request carries. Which country a site thinks the
        // reader is in is another matter -- that is read from the address the
        // request comes from, and no header changes it.
        preferences.intl_locale_override = "en-US".to_string();
        // What the page is told it is talking to. Servo's own string says Servo
        // as well as Firefox, and a good many sites read that and serve
        // something older or refuse outright; this is the string Firefox's own
        // nightly builds send.
        preferences.user_agent = FIREFOX_NIGHTLY.to_string();
        preferences.gfx_subpixel_text_antialiasing_enabled = false;
        let cores = std::thread::available_parallelism()
            .map(|cores| cores.get())
            .unwrap_or(4);
        preferences.layout_threads = cores.clamp(2, 8) as i64;
        // Whatever a machine's own answer, an environment saying otherwise wins:
        // this is how the two are told apart when measuring.
        if std::env::var("ZED_HTML_SUBPIXEL_TEXT").as_deref() == Ok("1") {
            preferences.gfx_subpixel_text_antialiasing_enabled = true;
        }
        // Pages are read in English wherever the machine happens to be. Without
        // this the engine negotiates language from the system locale, so the same
        // site answers in a different language in every country.
        preferences.intl_locale_override = READING_LOCALE.to_string();
        // A site's idea of where the reader is comes from the address its
        // requests arrive from. Somewhere to send them through is the only thing
        // that changes it, so the reader may name one.
        let through = options
            .proxy
            .clone()
            .or_else(|| std::env::var("ZED_HTML_PROXY").ok())
            .map(|through| through.trim().to_string())
            .filter(|through| !through.is_empty());
        if let Some(through) = through {
            preferences.network_http_proxy_uri = through.clone();
            preferences.network_https_proxy_uri = through.clone();
            log::info!("the engine reaches the network through {through}");
        }
        // The engine can answer a browser's own developer tools over the wire, as
        // Firefox's do. Off unless the reader asks for it: it is a port anything
        // on the machine could speak to.
        if let Some(port) = options.devtools_port.filter(|port| *port > 0) {
            preferences.devtools_server_enabled = true;
            preferences.devtools_server_listen_address = format!("127.0.0.1:{port}");
            log::info!("the engine answers developer tools on 127.0.0.1:{port}");
        }
        if let Ok(threads) = std::env::var("ZED_HTML_LAYOUT_THREADS")
            && let Ok(threads) = threads.parse::<i64>()
        {
            preferences.layout_threads = threads;
        }
        log::info!(
            "the engine lays pages out on {} threads with grids and columns, text edges {}",
            preferences.layout_threads,
            if preferences.gfx_subpixel_text_antialiasing_enabled {
                "coloured"
            } else {
                "grey"
            }
        );
        preferences
    }

    /// One live page. Holds its own surface, so two previews never draw over one
    /// another, and hands out the latest frame the engine painted.
    pub struct HtmlPage {
        engine: Rc<HtmlEngine>,
        rendering_context: Rc<GpuSurface>,
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
        /// Whether the engine has been told to hold this page back.
        throttled: bool,
        /// Whether the page has been told the editor is dark, once it has been
        /// told anything at all.
        dark: Option<bool>,
        /// Whether the window draws this page's own buffer. While it does, there
        /// is no reason to read the frame back into memory at all.
        shared: bool,
        /// Whether the window has looked at this page's buffer and refused it.
        /// Then frames are copied, and the buffer is never offered again.
        refused: bool,
        /// Whether a frame has been drawn but the graphics card has not yet
        /// reached the mark left after it, so the window may not read it.
        awaiting_the_card: std::cell::Cell<bool>,
        frame: Option<Arc<RenderImage>>,
        /// The document on disk. Servo loads by URL, and the file has to outlive
        /// the load.
        document: PageFile,
        /// The document before it, until the new one has finished loading.
        previous: Option<PageFile>,
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
            let rendering_context = std::panic::catch_unwind(|| GpuSurface::new(size))
                .map_err(|_| anyhow!("this platform has no rendering surface for HTML"))?
                .map_err(|error| anyhow!("no rendering surface for HTML: {error:?}"))?;
            let rendering_context = Rc::new(rendering_context);
            rendering_context
                .make_current()
                .map_err(|error| anyhow!("the HTML surface refused to bind: {error:?}"))?;

            let document = PageFile::write(&html, base_directory)?;
            let delegate = Rc::new(PageDelegate::default());
            let webview = {
                let servo = engine.servo.borrow();
                let servo = servo
                    .as_ref()
                    .ok_or_else(|| anyhow!("the HTML engine has already stopped"))?;
                WebViewBuilder::new(servo, rendering_context.clone())
                    .url(document.url()?)
                    .hidpi_scale_factor(euclid::Scale::new(scale))
                    .delegate(delegate.clone())
                    .build()
            };
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
                throttled: false,
                dark: None,
                shared: false,
                refused: false,
                awaiting_the_card: std::cell::Cell::new(false),
                frame: None,
                document,
                previous: None,
            })
        }

        /// Opens a page straight at an address, with no document of our own
        /// behind it. What it arrives at is somebody else's page, so our own
        /// script is put in once it has settled.
        pub fn open_at(
            url: url::Url,
            size: Size<Pixels>,
            scale: f32,
            cx: &mut App,
        ) -> Result<Self> {
            let mut page = Self::open(SharedString::default(), None, size, scale, cx)?;
            page.go_to(url);
            Ok(page)
        }

        /// Takes the page to an address. Everything else about the page stays:
        /// the same engine, the same surface, the same history.
        pub fn go_to(&mut self, url: url::Url) {
            // What it arrives at is not our document, and none of our script
            // will be in it until it has settled.
            self.delegate.needs_the_shim.set(true);
            self.webview.load(url);
            self.engine.nudge();
        }

        /// Fetches the page again, as a browser's refresh does.
        pub fn refresh(&self) {
            self.webview.reload();
            self.engine.nudge();
        }

        /// A step back through the pages the reader has been to, and forward
        /// again.
        pub fn go_back(&self) {
            self.webview.go_back(1);
            self.engine.nudge();
        }

        pub fn go_forward(&self) {
            self.webview.go_forward(1);
            self.engine.nudge();
        }

        /// Where the page is now, as the engine has it.
        pub fn address(&self) -> Option<url::Url> {
            self.webview.url()
        }

        /// Whether there is anywhere to go back to, or forward to.
        pub fn can_go(&self) -> (bool, bool) {
            let (behind, ahead) = self.delegate.history.get();
            (behind, ahead)
        }

        /// Points the page at a fresh document, keeping the same engine and
        /// surface: an edit reloads rather than starting a new browser.
        pub fn reload(&mut self, html: SharedString, base_directory: Option<&Path>) -> Result<()> {
            let document = PageFile::write(&html, base_directory)?;
            self.webview.load(document.url()?);
            // The old document is kept until the new one has arrived: a load is
            // not finished when it is asked for, and the page may still be
            // fetching a stylesheet or an image from beside the old file.
            self.previous = Some(std::mem::replace(&mut self.document, document));
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
            // Bound again: one turn of the engine runs every page there is, and
            // it leaves current whichever page it painted last. What follows
            // touches this page's own texture and buffers.
            self.bind();
            self.settled();
            // One drawing for each turn, however many times the engine said it
            // was ready during it.
            if self.delegate.frame_waiting.replace(false) {
                self.webview.paint();
                self.delegate.painted.set(true);
            }
            let painted = self.delegate.painted.replace(false);
            if self.shared {
                // The window samples the page's own buffer; copying it here
                // would be the very cost this avoids. What it does need is for
                // the drawing to be finished before it reads -- so the queue is
                // marked, and the frame is shown on the turn the card reaches
                // the mark rather than by standing and waiting for it.
                if painted {
                    self.rendering_context.seal_frame();
                    // The face just finished is what the window is handed; the
                    // next frame goes into the other one, so the page never
                    // draws over what the window is still reading.
                    self.rendering_context.turn_the_page();
                }
                // Taken first: `||` would skip it whenever a frame was painted
                // this turn, and a flag left standing reports a frame that is
                // not there on some later turn.
                let awaited = self.awaiting_the_card.take();
                if self.rendering_context.frame_is_drawn() {
                    return painted || awaited;
                }
                self.awaiting_the_card.set(true);
                self.engine.nudge();
                return false;
            }
            // Collected before the next is asked for: a frame asked for while
            // one is already on its way replaces it, and then the read that is
            // collected is the one just issued, which the card has had no time
            // for. That is the wait this is all meant to avoid.
            let collected = self.rendering_context.collect_frame();
            if painted {
                self.rendering_context.ask_for_frame();
            }
            if let Some(image) = collected {
                self.frame = Some(Arc::new(render_image(image)));
                // A frame was asked for just now, so there is something to come
                // back for even though this turn has something to show.
                if self.rendering_context.frame_on_the_way() {
                    self.engine.nudge();
                }
                return true;
            }
            if self.rendering_context.frame_on_the_way() {
                // A page at rest paints once and says nothing more, so the turn
                // that collects this frame has to be asked for.
                self.engine.nudge();
            }
            false
        }

        pub fn frame(&self) -> Option<Arc<RenderImage>> {
            self.frame.clone()
        }

        /// The engine behind this page, for whoever drives it.
        pub fn engine(&self) -> Rc<HtmlEngine> {
            self.engine.clone()
        }

        /// The frame the window can draw without a copy, when this machine
        /// allows it. `None` means the frame has to be handed over as pixels.
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        pub fn shared_frame(&mut self) -> Option<Arc<gpui::SharedFrame>> {
            if self.refused {
                return None;
            }
            let Some(frame) = self.rendering_context.shared_frame() else {
                // The surface has stopped lending its memory -- a buffer it
                // could not allocate, a driver that changed its mind. A page
                // that goes on as though it were still lent shows the window
                // nothing new ever again, so it goes back to copying.
                if self.shared {
                    log::info!("the page's memory is no longer lent, so frames are copied");
                    self.shared = false;
                }
                return None;
            };
            if frame.is_refused() {
                // The window looked at this buffer and could not draw it. Asking
                // again would only get the same answer.
                log::info!("the window will not draw the page's buffer, so frames are copied");
                self.refused = true;
                self.shared = false;
                return None;
            }
            if !self.shared {
                self.shared = true;
                // Whatever was copied before is not what the window draws now,
                // and the buffers frames were copied through are so much memory
                // nothing will read again.
                self.frame = None;
                self.rendering_context.stop_reading_back();
            }
            Some(frame)
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

        /// How far along the page is, as a fraction. The engine says only three
        /// things -- asked for, head parsed, all of it -- so this is coarse on
        /// purpose: a bar that moves in three steps is honest, one that crawls
        /// smoothly to ninety per cent is a lie.
        pub fn how_far_loaded(&self) -> Option<f32> {
            match self.delegate.load_status.get() {
                LoadStatus::Started => Some(0.15),
                LoadStatus::HeadParsed => Some(0.6),
                LoadStatus::Complete => None,
            }
        }

        /// Called on every turn: a page that has finished loading no longer
        /// needs the document it came from, and a page the reader has navigated
        /// to has none of our own script in it, so it gets it again.
        fn settled(&mut self) {
            if self.delegate.load_status.get() != LoadStatus::Complete {
                return;
            }
            self.previous = None;
            if self.delegate.needs_the_shim.replace(false) {
                self.evaluate(SELECTION_SHIM, |_| {});
            }
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
            self.engine.nudge();
        }

        pub fn mouse_down(&self, point: Point<Pixels>, button: MouseButton) {
            self.webview
                .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                    MouseButtonAction::Down,
                    button,
                    self.device(point),
                )));
            self.engine.nudge();
        }

        pub fn mouse_up(&self, point: Point<Pixels>, button: MouseButton) {
            self.webview
                .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                    MouseButtonAction::Up,
                    button,
                    self.device(point),
                )));
            self.engine.nudge();
        }

        /// A turn of the wheel.
        ///
        /// Told to the compositor rather than dispatched into the page: the
        /// compositor moves its own scroll node, which is a great deal less work
        /// than laying the page out around a wheel event. Only one of the two is
        /// sent -- sending both moved every page twice as far as it was told to.
        pub fn scrolled(&self, point: Point<Pixels>, delta: Point<Pixels>) {
            let (horizontal, vertical) = self.painted_scale();
            let across = f32::from(delta.x) * horizontal;
            let down = f32::from(delta.y) * vertical;
            if std::env::var("ZED_HTML_WHEEL_TO_PAGE").as_deref() == Ok("1") {
                self.webview
                    .notify_input_event(InputEvent::Wheel(WheelEvent::new(
                        WheelDelta {
                            x: across as f64,
                            y: down as f64,
                            z: 0.,
                            mode: WheelMode::DeltaPixel,
                        },
                        self.device(point),
                    )));
            } else {
                // The compositor counts the other way from a wheel: a positive
                // offset reveals what is below, where a wheel turned towards the
                // reader gives a negative one.
                self.webview.notify_scroll_event(
                    servo::Scroll::Delta(servo::WebViewVector::Device(euclid::Vector2D::new(
                        -across, -down,
                    ))),
                    self.device(point),
                );
            }
            self.engine.nudge();
        }

        /// Runs a script in the page and hands back what it evaluated to, as
        /// text. The answer comes through the engine's own event loop, so it
        /// arrives on a later turn rather than from this call.
        pub fn evaluate(&self, script: &str, deliver: impl FnOnce(String) + 'static) {
            self.engine.nudge();
            self.webview
                .evaluate_javascript(script, move |result| match result {
                    Ok(servo::JSValue::String(text)) => deliver(text),
                    Ok(other) => deliver(format!("{other:?}")),
                    Err(error) => {
                        log::warn!("the page would not run a script: {error:?}");
                        deliver(String::new())
                    }
                });
        }

        /// Asks the page where it stands: how far down it is scrolled, how tall
        /// the document is and how much of it is on screen, all in the page's
        /// own pixels. The answer comes back on a later turn.
        pub fn scroll_position(&self, deliver: impl FnOnce(f32, f32, f32) + 'static) {
            self.evaluate(
                "[window.scrollY, document.documentElement.scrollHeight, window.innerHeight]\
                 .join(',')",
                move |answer| {
                    let mut numbers = answer.split(',').map(|part| part.trim().parse::<f32>());
                    if let (Some(Ok(down)), Some(Ok(document)), Some(Ok(view))) =
                        (numbers.next(), numbers.next(), numbers.next())
                    {
                        deliver(down, document, view);
                    }
                },
            );
        }

        /// Takes the page to a position, as a drag of the scrollbar's thumb asks
        /// for. The engine scrolls pages itself, so this is the page's own doing
        /// rather than the compositor's.
        pub fn scroll_to(&self, down: f32) {
            self.evaluate(&format!("window.scrollTo(0, {down}), ''"), |_| {});
        }

        /// Looks for `query` in the page and takes the reader to the next place
        /// it appears, or the one before. The answer is which of them the reader
        /// is now at and how many there are, and it comes on a later turn.
        pub fn find(
            &self,
            query: &str,
            forward: bool,
            deliver: impl FnOnce(usize, usize) + 'static,
        ) {
            let query = serde_escape(query);
            self.evaluate(
                &format!(
                    "window.__zedSelection ? window.__zedSelection.find(\"{query}\", {forward}) \
                     : '0,0'"
                ),
                move |answer| {
                    let mut numbers = answer.split(',').map(|part| part.trim().parse::<usize>());
                    if let (Some(Ok(at)), Some(Ok(total))) = (numbers.next(), numbers.next()) {
                        deliver(at, total);
                    }
                },
            );
        }

        /// Puts the page back as it was before the search.
        pub fn stop_looking(&self) {
            self.evaluate(
                "window.__zedSelection ? window.__zedSelection.stopLooking() : 0, ''",
                |_| {},
            );
        }

        /// Asks the page a question a developer's panel needs answering: the
        /// tree it is made of, what its scripts have said, what it fetched, or
        /// what one element is. The answer is whatever the page returned, as
        /// text, on a later turn.
        pub fn ask_tools(&self, question: &str, deliver: impl FnOnce(String) + 'static) {
            self.evaluate(
                &format!("window.__zedTools ? window.__zedTools.{question} : ''"),
                deliver,
            );
        }

        /// Asks the page for the text the reader has selected.
        pub fn selected_text(&self, deliver: impl FnOnce(String) + 'static) {
            self.evaluate(
                "window.__zedSelection ? window.__zedSelection.text() : ''",
                deliver,
            );
        }

        /// Picks the whole page, as a drag from its first word to its last.
        pub fn select_all(&self) {
            self.evaluate(
                "window.__zedSelection ? window.__zedSelection.selectAll() : 0, ''",
                |_| {},
            );
        }

        /// Asks the page what it has under a point: the address of the nearest
        /// link, the address of the image, and what the reader has selected, as
        /// JSON. The answer comes back on a later turn.
        ///
        /// One script rather than three, because each answer arrives on a turn
        /// of the engine's own and three of them would arrive in any order --
        /// and a menu built from parts of two different clicks is worse than no
        /// menu at all. The nearest ancestor is asked for rather than the
        /// element itself: the page's own words are wrapped in spans for
        /// selection, so what is under the pointer inside a link is a span.
        pub fn what_is_under(&self, point: Point<Pixels>, deliver: impl FnOnce(String) + 'static) {
            let (x, y) = self.css_point(point);
            self.evaluate(
                &format!(
                    "(function(){{\
                       var at = document.elementFromPoint({x}, {y});\
                       var link = at && at.closest ? at.closest('a[href]') : null;\
                       var image = at && at.closest ? at.closest('img[src]') : null;\
                       var picked = window.__zedSelection ? window.__zedSelection.text() : '';\
                       return JSON.stringify({{\
                         link: link ? link.href : null,\
                         image: image ? (image.currentSrc || image.src) : null,\
                         selection: picked ? picked.slice(0, 500) : null\
                       }});\
                     }})()"
                ),
                deliver,
            );
        }

        /// Tells the page whether the editor is dark or light, so a page that
        /// asks -- through `prefers-color-scheme` -- is answered the same way
        /// the rest of the window is dressed.
        pub fn set_dark(&mut self, dark: bool) {
            if self.dark == Some(dark) {
                return;
            }
            self.dark = Some(dark);
            self.webview.notify_theme_change(if dark {
                servo::Theme::Dark
            } else {
                servo::Theme::Light
            });
            self.engine.nudge();
        }

        /// Lets a page that nobody is looking at stop working: the engine holds
        /// back its animations and timers, the way a browser does with a tab in
        /// the background.
        pub fn set_throttled(&mut self, throttled: bool) {
            if self.throttled == throttled {
                return;
            }
            self.throttled = throttled;
            self.webview.set_throttled(throttled);
        }

        pub fn key(&self, event: keyboard_types::KeyboardEvent) {
            self.webview
                .notify_input_event(InputEvent::Keyboard(servo::KeyboardEvent::new(event)));
            self.engine.nudge();
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

        /// Where a position in editor pixels lands in the page's own CSS
        /// pixels, which is what a script asking about a point is answered in.
        ///
        /// Normally the same number, since the page is laid out at the scale the
        /// surface is drawn at. The two part company where the surface hit its
        /// ceiling: then the picture is stretched over the view and the page is
        /// smaller than it looks.
        fn css_point(&self, point: Point<Pixels>) -> (f32, f32) {
            let (horizontal, vertical) = self.painted_scale();
            (
                f32::from(point.x).max(0.) * horizontal / self.scale,
                f32::from(point.y).max(0.) * vertical / self.scale,
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

    /// What the reader typed, as a JavaScript string may hold it. Anything that
    /// would end the string early, or start a new line in the middle of it, is
    /// written the way JavaScript writes it.
    fn serde_escape(text: &str) -> String {
        let mut escaped = String::with_capacity(text.len());
        for character in text.chars() {
            match character {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\u{2028}' => escaped.push_str("\\u2028"),
                '\u{2029}' => escaped.push_str("\\u2029"),
                other => escaped.push(other),
            }
        }
        escaped
    }

    /// A display scale that cannot turn a page into a surface of no pixels or
    /// of far too many.
    fn usable_scale(scale: f32) -> f32 {
        if scale.is_finite() {
            scale.clamp(0.5, 4.)
        } else {
            1.
        }
    }

    fn surface_size(size: Size<Pixels>, scale: f32) -> PhysicalSize<u32> {
        let device = |length: Pixels| ((f32::from(length) * scale).max(1.) as u32).min(MAX_SURFACE);
        PhysicalSize {
            width: device(size.width),
            height: device(size.height),
        }
    }

    /// The frame as the window holds it. Its pixels already come back blue
    /// first, which is the order `RenderImage` is uploaded in.
    fn render_image(image: RgbaImage) -> RenderImage {
        RenderImage::new(SmallVec::from_elem(image::Frame::new(image), 1))
    }

    /// The document, on disk, for Servo to load over `file://`. A `<base href>`
    /// is what makes the page's own relative links resolve the way they do in a
    /// browser.
    struct PageFile {
        _directory: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    /// Mouse selection, implemented in the page itself because the engine has no
    /// selection to offer outside form fields.
    const SELECTION_SHIM: &str = include_str!("selection.js");

    impl PageFile {
        fn write(html: &str, base_directory: Option<&Path>) -> Result<Self> {
            let directory = tempfile::tempdir().context("creating a directory for the page")?;
            let path = directory.path().join("page.html");
            let document =
                match base_directory.and_then(|base| url::Url::from_directory_path(base).ok()) {
                    Some(base) => format!("<base href=\"{base}\">\n{html}"),
                    None => html.to_string(),
                };
            // First, so that what the page's own scripts say as they run is kept:
            // the shim hooks the console, and anything said before it is in
            // place is said to nobody. It touches no element until the page has
            // loaded, so being ahead of the document costs it nothing.
            let document = format!("<script>{SELECTION_SHIM}</script>\n{document}");
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
        /// Whether the engine has a frame that has not been drawn yet.
        frame_waiting: Cell<bool>,
        painted: Cell<bool>,
        load_status: Cell<LoadStatus>,
        link_for_new_tab: RefCell<Option<url::Url>>,
        /// Set when the page has gone somewhere of its own accord. What it
        /// arrives at has none of our script in it, so it has to be put back.
        needs_the_shim: Cell<bool>,
        /// Whether there is anywhere behind and ahead of where the page is now.
        history: Cell<(bool, bool)>,
    }

    impl Default for PageDelegate {
        fn default() -> Self {
            Self {
                frame_waiting: Cell::new(false),
                painted: Cell::new(false),
                load_status: Cell::new(LoadStatus::Started),
                link_for_new_tab: RefCell::new(None),
                needs_the_shim: Cell::new(false),
                history: Cell::new((false, false)),
            }
        }
    }

    impl WebViewDelegate for PageDelegate {
        /// The engine has a frame to show. It is not drawn here: this is called
        /// from the middle of the engine's own turn, and Servo's own shell only
        /// asks its window for a redraw at this point. Drawing once per turn of
        /// the pump instead means one render for each frame the editor shows,
        /// however many times the engine says it is ready.
        fn notify_new_frame_ready(&self, _webview: WebView) {
            self.frame_waiting.set(true);
        }

        fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
            self.load_status.set(status);
        }

        fn notify_history_changed(
            &self,
            _webview: WebView,
            entries: Vec<url::Url>,
            current: usize,
        ) {
            self.history.set((current > 0, current + 1 < entries.len()));
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
            self.needs_the_shim.set(true);
            navigation_request.allow();
        }
    }

    #[derive(Clone)]
    struct Waker(async_channel::Sender<()>);

    impl EventLoopWaker for Waker {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }

        fn wake(&self) {
            // Called from the engine's own threads. A full channel already
            // carries the same message, and nothing here may block.
            self.0.try_send(()).ok();
        }
    }
}

#[cfg(feature = "servo")]
pub use engine::{EngineOptions, HtmlEngine, HtmlPage, set_engine_options};
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
