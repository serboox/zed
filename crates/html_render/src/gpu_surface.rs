use std::cell::{Cell, RefCell};
use std::rc::Rc;

use std::sync::Arc;

use dpi::PhysicalSize;
use euclid::Size2D;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use gpui::SharedFrame;

#[cfg(target_os = "linux")]
use crate::dma_buf_export::DmaBufExporter;
#[cfg(target_os = "linux")]
use crate::shared_buffer::{FORMAT_ABGR8888, SharedBuffer, SharedBuffers};
#[cfg(target_os = "windows")]
use crate::shared_texture::{FORMAT_ABGR8888, SharedBuffer, SharedBuffers};
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use nothing::{SharedBuffer, SharedBuffers};
use servo::{DeviceIntRect, RenderingContext, RgbaImage};
use surfman::{
    Adapter, Connection, Context, ContextAttributeFlags, ContextAttributes, Device, Error, GLApi,
    GLVersion, Surface, SurfaceAccess, SurfaceTexture, SurfaceType,
};

/// A surface the graphics card draws the page into.
///
/// Servo ships two of these and neither fits an editor pane. Its
/// `SoftwareRenderingContext` -- "this will generally have bad performance", in
/// Servo's own words -- asks surfman for a *software* adapter, so the page is
/// rasterized by the processor while the graphics card sits idle. Its
/// `WindowRenderingContext` wants a window of its own, which a pane is not.
///
/// This is the same construction as Servo's own, with the one difference that
/// matters: the adapter is the real graphics card.
pub(crate) struct GpuSurface {
    gleam_gl: Rc<dyn gleam::gl::Gl>,
    glow_gl: Arc<glow::Context>,
    device: RefCell<Device>,
    context: RefCell<Context>,
    size: Cell<PhysicalSize<u32>>,
    /// What the page is drawn into. It is ours rather than surfman's, because a
    /// texture we own is a texture we can hand to the window as it stands.
    target: RefCell<RenderTarget>,
    /// Where the page's own memory comes from, when the driver will allocate
    /// something the window can read. Absent when it will not.
    buffers: Option<SharedBuffers>,
    /// How a texture becomes something the window can sample. Absent when this
    /// driver has no way to hand a buffer over, and then frames are copied.
    #[cfg(target_os = "linux")]
    exporter: Option<DmaBufExporter>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    /// Whether handing frames over has already been found impossible on this
    /// machine. Asking again every frame costs an image each time and the answer
    /// does not change until the page is resized.
    cannot_share: Cell<bool>,
    /// The byte order this driver reads pixels back in, once asked.
    read_format: Cell<Option<u32>>,
    /// A mark left in the graphics card's queue after the page was drawn. The
    /// window may not read the buffer until the card has reached it.
    sealed: Cell<gleam::gl::GLsync>,
    /// How many turns the card has been asked about the current mark.
    asked: Cell<u32>,
    /// Where a frame is read into while the graphics card is still working on
    /// it. Reading straight into memory makes the processor wait for the card to
    /// finish; reading into one of these does not, and the frame is collected on
    /// the next turn from the other one.
    readback: RefCell<Readback>,
}

/// Two buffers the graphics card copies frames into on its own time, and which
/// of them the next frame goes to.
#[derive(Default)]
struct Readback {
    buffers: Option<[u32; 2]>,
    /// Which buffer the next frame is asked for.
    next: usize,
    /// How many bytes each holds, so a resized page grows them.
    capacity: usize,
    /// The frame that has been asked for and not yet collected.
    waiting: Option<Waiting>,
}

/// A frame the graphics card is filling in, and what it will take to turn it
/// into something the window can upload.
struct Waiting {
    slot: usize,
    width: usize,
    height: usize,
    /// Whether the driver read the pixels red first. The window's textures want
    /// blue first, so those frames are put right while they are turned over.
    swap_red_and_blue: bool,
}

/// A texture and the framebuffer that draws into it. When the memory behind the
/// texture came from the driver's allocator, the window can be handed it as it
/// stands instead of being sent a copy.
struct Face {
    texture: u32,
    framebuffer: u32,
    shared: Option<SharedBuffer>,
    /// The frame the window is handed for this face, made once and handed out
    /// every time this face comes round again.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    frame: RefCell<Option<Arc<SharedFrame>>>,
}

/// What the page draws into.
///
/// When the window is drawing the page's own memory there are two of these and
/// the page draws into them by turns: the window is still sampling the frame it
/// was last handed, and drawing over that very memory is how a page tears -- or,
/// where the driver orders it for us, how the page ends up waiting on the window
/// before it may draw at all. When frames are copied instead, one is enough.
struct RenderTarget {
    faces: Vec<Face>,
    /// Which face the engine is drawing into now.
    drawing: Cell<usize>,
    /// Which face was last finished, and so is the one to hand over.
    drawn: Cell<usize>,
}

impl RenderTarget {
    fn new(
        gl: &Rc<dyn gleam::gl::Gl>,
        size: PhysicalSize<u32>,
        buffers: Option<&SharedBuffers>,
    ) -> Option<Self> {
        let wanted = if buffers.is_some() { 2 } else { 1 };
        let mut faces = Vec::with_capacity(wanted);
        while faces.len() < wanted {
            match Face::new(gl, size, buffers) {
                Some(face) => faces.push(face),
                None => {
                    for face in &faces {
                        face.discard(gl, buffers);
                    }
                    // A shared buffer was the reason it would not do; an
                    // ordinary texture still might.
                    return buffers.and_then(|_| Self::new(gl, size, None));
                }
            }
        }
        Some(Self {
            faces,
            drawing: Cell::new(0),
            drawn: Cell::new(0),
        })
    }

    fn drawing(&self) -> &Face {
        let at = self.drawing.get().min(self.faces.len() - 1);
        &self.faces[at]
    }

    fn drawn(&self) -> &Face {
        let at = self.drawn.get().min(self.faces.len() - 1);
        &self.faces[at]
    }

    /// Whether the page has drawn into a face and moved on from it. Until it has,
    /// the face the window would be handed is the one still being drawn into.
    #[cfg(target_os = "windows")]
    fn nothing_turned_yet(&self) -> bool {
        self.faces.len() > 1 && self.drawn.get() == self.drawing.get()
    }

    /// Says the page has been drawn: what was being drawn into is what the
    /// window is handed, and the next frame goes somewhere else.
    fn turn(&self) {
        self.drawn.set(self.drawing.get());
        self.drawing
            .set((self.drawing.get() + 1) % self.faces.len());
    }

    fn discard(&self, gl: &Rc<dyn gleam::gl::Gl>, buffers: Option<&SharedBuffers>) {
        for face in &self.faces {
            face.discard(gl, buffers);
        }
    }

    /// Gives back the memory the driver allocated. This asks nothing of OpenGL,
    /// so it is also the right thing to do when the context cannot be made
    /// current any more.
    fn discard_shared(&self, buffers: Option<&SharedBuffers>) {
        for face in &self.faces {
            face.discard_shared(buffers);
        }
    }
}

impl Face {
    fn new(
        gl: &Rc<dyn gleam::gl::Gl>,
        size: PhysicalSize<u32>,
        buffers: Option<&SharedBuffers>,
    ) -> Option<Self> {
        let shared = buffers.and_then(|buffers| {
            let buffer = buffers.allocate(size.width, size.height)?;
            Some((buffers, buffer))
        });
        // A buffer was asked for and refused: the whole target has to be built
        // the other way round instead of half one and half the other.
        if buffers.is_some() && shared.is_none() {
            return None;
        }
        let give_up = |shared: &Option<(&SharedBuffers, SharedBuffer)>| {
            if let Some((buffers, buffer)) = shared {
                buffers.discard(buffer);
            }
        };
        let textures = gl.gen_textures(1);
        let Some(&texture) = textures.first() else {
            give_up(&shared);
            return None;
        };
        gl.bind_texture(gleam::gl::TEXTURE_2D, texture);
        match &shared {
            Some((buffers, buffer)) => {
                if !buffers.bind_to_texture(buffer, texture) {
                    gl.delete_textures(&[texture]);
                    give_up(&shared);
                    return None;
                }
            }
            None => gl.tex_image_2d(
                gleam::gl::TEXTURE_2D,
                0,
                gleam::gl::RGBA8 as i32,
                size.width as i32,
                size.height as i32,
                0,
                gleam::gl::RGBA,
                gleam::gl::UNSIGNED_BYTE,
                None,
            ),
        }
        // No mipmaps and no wrapping: the page is sampled once, at its own size.
        for (name, value) in [
            (gleam::gl::TEXTURE_MIN_FILTER, gleam::gl::LINEAR),
            (gleam::gl::TEXTURE_MAG_FILTER, gleam::gl::LINEAR),
            (gleam::gl::TEXTURE_WRAP_S, gleam::gl::CLAMP_TO_EDGE),
            (gleam::gl::TEXTURE_WRAP_T, gleam::gl::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i(gleam::gl::TEXTURE_2D, name, value as i32);
        }

        let framebuffers = gl.gen_framebuffers(1);
        let Some(&framebuffer) = framebuffers.first() else {
            gl.delete_textures(&[texture]);
            give_up(&shared);
            return None;
        };
        gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, framebuffer);
        gl.framebuffer_texture_2d(
            gleam::gl::FRAMEBUFFER,
            gleam::gl::COLOR_ATTACHMENT0,
            gleam::gl::TEXTURE_2D,
            texture,
            0,
        );
        let status = gl.check_frame_buffer_status(gleam::gl::FRAMEBUFFER);
        if status != gleam::gl::FRAMEBUFFER_COMPLETE {
            log::error!("the page has no framebuffer to draw into: status {status:#x}");
            gl.delete_framebuffers(&[framebuffer]);
            gl.delete_textures(&[texture]);
            give_up(&shared);
            return None;
        }
        Some(Self {
            texture,
            framebuffer,
            shared: shared.map(|(_, buffer)| buffer),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            frame: RefCell::new(None),
        })
    }

    fn discard(&self, gl: &Rc<dyn gleam::gl::Gl>, buffers: Option<&SharedBuffers>) {
        gl.delete_framebuffers(&[self.framebuffer]);
        // Before the texture goes: where a driver pairs an OpenGL texture with one
        // of its own, that pair has to be broken while both halves still exist.
        // Giving the memory up first is safe either way -- a texture made from a
        // shared image keeps the image's contents once the image itself has gone.
        self.discard_shared(buffers);
        gl.delete_textures(&[self.texture]);
    }

    fn discard_shared(&self, buffers: Option<&SharedBuffers>) {
        if let (Some(buffers), Some(buffer)) = (buffers, self.shared.as_ref()) {
            buffers.discard(buffer);
        }
    }
}

impl GpuSurface {
    /// Opens a surface of `size`, on the best adapter this machine will give.
    /// Each candidate is tried all the way through: an adapter that reports
    /// itself present can still fail to make a context, and a preview is worth
    /// more than insisting on the fastest one.
    pub(crate) fn new(size: PhysicalSize<u32>) -> Result<Self, Error> {
        let connection = Connection::new()?;
        let candidates: [(&str, fn(&Connection) -> Result<Adapter, Error>); 3] = [
            ("the graphics card", Connection::create_hardware_adapter),
            ("the default adapter", Connection::create_adapter),
            ("a software rasterizer", Connection::create_software_adapter),
        ];
        let mut last = Error::Failed;
        for (what, choose) in candidates {
            let adapter = match choose(&connection) {
                Ok(adapter) => adapter,
                Err(error) => {
                    log::warn!("the HTML engine cannot use {what}: {error:?}");
                    last = error;
                    continue;
                }
            };
            match Self::build(&connection, &adapter, size) {
                Ok(surface) => {
                    log::info!("the HTML engine draws on {what}");
                    return Ok(surface);
                }
                Err(error) => {
                    log::warn!("the HTML engine could not draw on {what}: {error:?}");
                    last = error;
                }
            }
        }
        Err(last)
    }

    fn build(
        connection: &Connection,
        adapter: &Adapter,
        size: PhysicalSize<u32>,
    ) -> Result<Self, Error> {
        let device = connection.create_device(adapter)?;

        let gl_api = connection.gl_api();
        let version = match gl_api {
            GLApi::GLES => GLVersion { major: 3, minor: 0 },
            GLApi::GL => GLVersion { major: 3, minor: 2 },
        };
        let descriptor = device.create_context_descriptor(&ContextAttributes {
            flags: ContextAttributeFlags::ALPHA
                | ContextAttributeFlags::DEPTH
                | ContextAttributeFlags::STENCIL,
            version,
        })?;
        let mut context = device.create_context(&descriptor, None)?;

        // The only way to reach an OpenGL implementation is through the pointers
        // the driver hands out for its own functions.
        #[allow(unsafe_code)]
        let (gleam_gl, glow_gl) = unsafe {
            let address = |name: &str| device.get_proc_address(&context, name);
            let gleam_gl: Rc<dyn gleam::gl::Gl> = match gl_api {
                GLApi::GL => gleam::gl::GlFns::load_with(address),
                GLApi::GLES => gleam::gl::GlesFns::load_with(address),
            };
            let glow_gl = glow::Context::from_loader_function(address);
            (gleam_gl, Arc::new(glow_gl))
        };

        let surface = device.create_surface(
            &context,
            SurfaceAccess::GPUOnly,
            SurfaceType::Generic {
                size: physical(size),
            },
        )?;
        if let Err((error, mut surface)) = device.bind_surface_to_context(&mut context, surface) {
            device.destroy_surface(&mut context, &mut surface).ok();
            device.destroy_context(&mut context).ok();
            return Err(error);
        }
        device.make_context_current(&context)?;

        // The surfman surface exists so the context has something to be current
        // with; the page is drawn into a texture of our own, beside it.
        let address = |name: &str| device.get_proc_address(&context, name);
        let buffers = SharedBuffers::new(&address, &gleam_gl, &device);
        let target = RenderTarget::new(&gleam_gl, size, buffers.as_ref()).ok_or(Error::Failed)?;
        #[cfg(target_os = "linux")]
        let exporter = DmaBufExporter::new(address);
        #[cfg(target_os = "linux")]
        if exporter.is_none() && buffers.is_none() {
            log::info!("this driver cannot hand frames over, so they will be copied");
        }
        #[cfg(target_os = "windows")]
        if buffers.is_none() {
            log::info!("this driver cannot hand frames over, so they will be copied");
        }

        Ok(Self {
            gleam_gl,
            glow_gl,
            device: RefCell::new(device),
            context: RefCell::new(context),
            size: Cell::new(size),
            target: RefCell::new(target),
            buffers,
            #[cfg(target_os = "linux")]
            exporter,
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            cannot_share: Cell::new(false),
            read_format: Cell::new(None),
            sealed: Cell::new(std::ptr::null()),
            asked: Cell::new(0),
            readback: RefCell::new(Readback::default()),
        })
    }

    /// The frame the window may sample directly, if this machine allows it.
    ///
    /// This is the face the page has just finished, not the one it is drawing
    /// into now. Each face keeps its own, so the two are handed out by turns and
    /// the window has them both.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub(crate) fn shared_frame(&self) -> Option<Arc<SharedFrame>> {
        if let Some(shared) = self.target.borrow().drawn().frame.borrow().as_ref() {
            return Some(shared.clone());
        }
        if self.cannot_share.get() {
            return None;
        }
        // An escape hatch, and the way the two paths are compared: a machine
        // where lending the buffer goes wrong can be told to copy instead.
        if std::env::var("ZED_HTML_PAGE_SHARING").as_deref() == Ok("0") {
            log::info!("frame sharing is switched off, so frames are copied");
            self.cannot_share.set(true);
            return None;
        }
        // Where the two APIs take turns holding a texture, the window may not be
        // handed one OpenGL still holds -- and until the page has moved on from a
        // face, the face it would be handed is the one it is drawing into.
        #[cfg(target_os = "windows")]
        {
            let nothing_turned_yet = self.target.borrow().nothing_turned_yet();
            if nothing_turned_yet {
                self.turn_the_page();
            }
        }
        let size = self.size.get();
        let frame = match self.lend_own_memory(size) {
            Some(frame) => frame,
            // Nothing was allocated to be shared, but a driver that lays its own
            // textures out plainly can still hand this one over.
            None => match self.export_the_texture(size) {
                Some(frame) => frame,
                None => {
                    self.cannot_share.set(true);
                    return None;
                }
            },
        };
        let frame = Arc::new(frame);
        *self.target.borrow().drawn().frame.borrow_mut() = Some(frame.clone());
        log::info!(
            "the page's frames are handed to the window as they are, {}x{}",
            size.width,
            size.height
        );
        Some(frame)
    }

    /// Says the page has been drawn, so the next frame goes into the other face
    /// and the one just finished is what the window is handed.
    pub(crate) fn turn_the_page(&self) {
        let target = self.target.borrow();
        // Where a face is a texture two graphics APIs share, which of them may
        // touch it is a lock rather than a convention: the one just drawn goes to
        // the window, and the one about to be drawn comes back to OpenGL.
        #[cfg(target_os = "windows")]
        if let Some(buffers) = self.buffers.as_ref() {
            if let Some(buffer) = target.drawing().shared.as_ref() {
                buffers.lend(buffer);
            }
            target.turn();
            if let Some(buffer) = target.drawing().shared.as_ref()
                && !buffers.take_back(buffer)
            {
                log::warn!("the page has nowhere to draw its next frame");
            }
            return;
        }
        target.turn();
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    /// The page's own memory, when it was allocated to be shared. The window is
    /// given a name of its own for it; the page keeps drawing into the memory
    /// both now name.
    fn lend_own_memory(&self, size: PhysicalSize<u32>) -> Option<SharedFrame> {
        let target = self.target.borrow();
        let buffer = target.drawn().shared.as_ref()?;
        Some(SharedFrame {
            descriptor: buffer.share()?,
            width: size.width,
            height: size.height,
            buffer_width: buffer.width,
            stride: buffer.stride,
            offset: buffer.offset,
            format: FORMAT_ABGR8888,
            // The page is drawn with OpenGL, which starts at the bottom.
            bottom_up: true,
            modifier: 0,
            refused: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Windows has nothing to export: a face there is already a texture the
    /// window can open, so a face without one means the driver would not make it.
    #[cfg(target_os = "windows")]
    fn export_the_texture(&self, _size: PhysicalSize<u32>) -> Option<SharedFrame> {
        None
    }

    #[cfg(target_os = "linux")]
    /// The texture the driver made for itself, if it will hand it over.
    fn export_the_texture(&self, size: PhysicalSize<u32>) -> Option<SharedFrame> {
        let exported = self
            .exporter
            .as_ref()?
            .export(self.target.borrow().drawn().texture)?;
        // The driver's own texture may have longer rows than the page is wide.
        // The window works a row's length out from the width it is given, so it
        // is told how wide the rows really are, and which part of them is the
        // page.
        let across = exported.stride / 4;
        if exported.stride % 4 != 0 || across < size.width {
            log::info!(
                "the page's texture has {}-byte rows, which do not hold {} pixels",
                exported.stride,
                size.width
            );
            return None;
        }
        Some(SharedFrame {
            descriptor: exported.descriptor,
            width: size.width,
            height: size.height,
            buffer_width: across,
            stride: exported.stride,
            offset: exported.offset,
            format: exported.format,
            bottom_up: true,
            modifier: exported.modifier,
            refused: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn framebuffer(&self) -> u32 {
        self.target.borrow().drawing().framebuffer
    }

    /// Which byte order to read pixels back in. The window's textures want blue
    /// first, so a driver that will read that way saves a pass over every pixel
    /// of every frame; whether it will is settled by asking it for one pixel and
    /// seeing whether it complains.
    fn read_format(&self) -> u32 {
        if let Some(format) = self.read_format.get() {
            return format;
        }
        let gl = &self.gleam_gl;
        gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, self.framebuffer());
        gl.bind_vertex_array(0);
        gl.bind_buffer(gleam::gl::PIXEL_PACK_BUFFER, 0);
        // Whatever went wrong before this is not what is being asked about. A
        // driver that answers with an error every time is not worth waiting on.
        drain_errors(gl);
        let probe = gl.read_pixels(0, 0, 1, 1, gleam::gl::BGRA, gleam::gl::UNSIGNED_BYTE);
        let format = if gl.get_error() == gleam::gl::NO_ERROR && probe.len() == 4 {
            gleam::gl::BGRA
        } else {
            gleam::gl::RGBA
        };
        log::info!(
            "the page's frames are read {}",
            if format == gleam::gl::BGRA {
                "in the window's own order"
            } else {
                "red first, and turned round as they are collected"
            }
        );
        self.read_format.set(Some(format));
        format
    }

    /// Asks the graphics card for the frame without waiting for it. It fills a
    /// buffer while the editor gets on with its work, and the frame is collected
    /// on a later turn.
    pub(crate) fn ask_for_frame(&self) {
        let (width, height) = {
            let size = self.size.get();
            (size.width as i32, size.height as i32)
        };
        if width <= 0 || height <= 0 {
            return;
        }
        let wanted = width as usize * height as usize * 4;
        let format = self.read_format();
        let gl = &self.gleam_gl;
        let mut readback = self.readback.borrow_mut();

        gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, self.framebuffer());
        gl.bind_vertex_array(0);

        let buffers = match readback.buffers {
            Some(buffers) if readback.capacity == wanted => buffers,
            existing => {
                if let Some(existing) = existing {
                    gl.delete_buffers(&existing);
                }
                let made = gl.gen_buffers(2);
                let (Some(&first), Some(&second)) = (made.first(), made.get(1)) else {
                    return;
                };
                for buffer in [first, second] {
                    gl.bind_buffer(gleam::gl::PIXEL_PACK_BUFFER, buffer);
                    gl.buffer_data_untyped(
                        gleam::gl::PIXEL_PACK_BUFFER,
                        wanted as isize,
                        std::ptr::null(),
                        gleam::gl::STREAM_READ,
                    );
                }
                readback.buffers = Some([first, second]);
                readback.capacity = wanted;
                readback.waiting = None;
                [first, second]
            }
        };

        let slot = readback.next;
        gl.bind_buffer(gleam::gl::PIXEL_PACK_BUFFER, buffers[slot]);
        #[allow(unsafe_code)]
        unsafe {
            gl.read_pixels_into_pbo(0, 0, width, height, format, gleam::gl::UNSIGNED_BYTE);
        }
        let error = gl.get_error();
        gl.bind_buffer(gleam::gl::PIXEL_PACK_BUFFER, 0);
        if error != gleam::gl::NO_ERROR {
            log::warn!("the page's surface could not be read: GL error {error:#x}");
            // Nothing was asked for, so there will be nothing to collect.
            return;
        }
        readback.waiting = Some(Waiting {
            slot,
            width: width as usize,
            height: height as usize,
            swap_red_and_blue: format == gleam::gl::RGBA,
        });
        readback.next = 1 - slot;
    }

    /// Collects a frame asked for earlier, if one is ready. The graphics card
    /// has had a turn of the loop to finish it.
    ///
    /// The pixels come back in the order the window uploads them, blue first,
    /// which is what `RenderImage` holds despite the name of its container.
    pub(crate) fn collect_frame(&self) -> Option<RgbaImage> {
        let gl = &self.gleam_gl;
        let mut readback = self.readback.borrow_mut();
        let waiting = readback.waiting.take()?;
        let Waiting {
            slot,
            width,
            height,
            swap_red_and_blue,
        } = waiting;
        let buffers = readback.buffers?;
        let wanted = width * height * 4;

        gl.bind_buffer(gleam::gl::PIXEL_PACK_BUFFER, buffers[slot]);
        let mapped = gl.map_buffer_range(
            gleam::gl::PIXEL_PACK_BUFFER,
            0,
            wanted as isize,
            gleam::gl::MAP_READ_BIT,
        );
        if mapped.is_null() {
            log::warn!("the page's frame could not be collected");
            gl.bind_buffer(gleam::gl::PIXEL_PACK_BUFFER, 0);
            return None;
        }
        #[allow(unsafe_code)]
        let rows = unsafe { std::slice::from_raw_parts(mapped as *const u8, wanted).to_vec() };
        gl.unmap_buffer(gleam::gl::PIXEL_PACK_BUFFER);
        gl.bind_buffer(gleam::gl::PIXEL_PACK_BUFFER, 0);
        turned_over(rows, width, height, swap_red_and_blue)
    }

    /// Whether a frame has been asked for and not yet collected.
    pub(crate) fn frame_on_the_way(&self) -> bool {
        self.readback.borrow().waiting.is_some()
    }

    /// Gives back the buffers frames were copied through. The page draws where
    /// the window can read it now, so nothing will read through them again, and
    /// at the size of a preview they are worth several megabytes each.
    pub(crate) fn stop_reading_back(&self) {
        let mut readback = self.readback.borrow_mut();
        if let Some(buffers) = readback.buffers.take() {
            self.gleam_gl.delete_buffers(&buffers);
        }
        *readback = Readback::default();
    }

    /// Marks the graphics card's queue after the page. The window reads this
    /// buffer with a different API, which knows nothing of OpenGL's queue, so
    /// the drawing has to be done before it looks -- but waiting for it here
    /// would stop the editor's own thread until the card caught up, which is
    /// the very cost this is meant to avoid.
    pub(crate) fn seal_frame(&self) {
        self.forget_seal();
        let sealed = self
            .gleam_gl
            .fence_sync(gleam::gl::SYNC_GPU_COMMANDS_COMPLETE, 0);
        if sealed.is_null() {
            // Without a mark there is nothing to wait on, so the old, blunt way
            // is the only way to keep the window from reading too early.
            self.gleam_gl.finish();
            return;
        }
        // The card is told there is work to get on with; nobody waits.
        self.gleam_gl.flush();
        self.sealed.set(sealed);
    }

    /// Whether the graphics card has reached the mark, asked without waiting.
    /// `true` also when there is no mark to wait for.
    pub(crate) fn frame_is_drawn(&self) -> bool {
        let sealed = self.sealed.get();
        if sealed.is_null() {
            return true;
        }
        // A card that is asked often enough and never answers would keep the
        // page from ever showing a frame, so after a few turns the old, blunt
        // way is used instead of asking again.
        const ASK_AT_MOST: u32 = 8;
        if self.asked.get() >= ASK_AT_MOST {
            log::debug!("the page's frame is waited for outright: the mark was not reached");
            self.gleam_gl.finish();
            self.forget_seal();
            return true;
        }
        self.asked.set(self.asked.get() + 1);
        match self.gleam_gl.client_wait_sync(sealed, 0, 0) {
            gleam::gl::ALREADY_SIGNALED | gleam::gl::CONDITION_SATISFIED => {
                self.forget_seal();
                true
            }
            gleam::gl::TIMEOUT_EXPIRED => false,
            // A mark that cannot be asked about is worse than none: it would
            // hold the page back for ever.
            _ => {
                self.forget_seal();
                true
            }
        }
    }

    fn forget_seal(&self) {
        self.asked.set(0);
        let sealed = self.sealed.replace(std::ptr::null());
        if !sealed.is_null() {
            self.gleam_gl.delete_sync(sealed);
        }
    }
}

impl Drop for GpuSurface {
    fn drop(&mut self) {
        let device = self.device.borrow();
        let mut context = self.context.borrow_mut();
        // Deleting a texture asks whichever context is current, and with a
        // second preview open that is somebody else's. This one is made current
        // first, or its own resources are not the ones that go.
        match device.make_context_current(&context) {
            Ok(()) => {
                self.forget_seal();
                self.target
                    .borrow()
                    .discard(&self.gleam_gl, self.buffers.as_ref());
                if let Some(buffers) = self.readback.borrow().buffers {
                    self.gleam_gl.delete_buffers(&buffers);
                }
            }
            Err(error) => {
                log::warn!("the page's own context would not come back: {error:?}");
                // Whatever OpenGL holds goes with the context; the driver's own
                // buffer does not, and the allocator is about to be closed.
                self.target.borrow().discard_shared(self.buffers.as_ref());
            }
        }
        device.destroy_context(&mut context).ok();
    }
}

impl RenderingContext for GpuSurface {
    fn prepare_for_rendering(&self) {
        self.gleam_gl
            .bind_framebuffer(gleam::gl::FRAMEBUFFER, self.framebuffer());
    }

    /// A frame, read the plain way: ask and wait. Nothing in the editor uses
    /// this -- the page is read a turn at a time instead, so the processor never
    /// waits on the graphics card -- but the engine's own interface offers it and
    /// it has to mean what it says.
    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        let width = source_rectangle.width();
        let height = source_rectangle.height();
        if width <= 0 || height <= 0 {
            return None;
        }
        self.gleam_gl
            .bind_framebuffer(gleam::gl::FRAMEBUFFER, self.framebuffer());
        self.gleam_gl.bind_vertex_array(0);
        let rows = self.gleam_gl.read_pixels(
            source_rectangle.min.x,
            source_rectangle.min.y,
            width,
            height,
            gleam::gl::RGBA,
            gleam::gl::UNSIGNED_BYTE,
        );
        flipped(rows, width as usize, height as usize)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 || size == self.size.get() {
            return;
        }
        // Only the texture the page is drawn into changes. The surfman surface
        // underneath is never drawn to -- and could not be resized anyway, since
        // a surface with no window attached refuses.
        if let Err(error) = self.make_current() {
            log::error!("the page's surface would not become current to resize: {error:?}");
            return;
        }
        let Some(target) = RenderTarget::new(&self.gleam_gl, size, self.buffers.as_ref()) else {
            return;
        };
        // A frame asked for at the old size is not worth collecting, and the
        // buffer it was going into is about to be the wrong size.
        self.readback.borrow_mut().waiting = None;
        let previous = std::mem::replace(&mut *self.target.borrow_mut(), target);
        previous.discard(&self.gleam_gl, self.buffers.as_ref());
        // What the window was lent belonged to the old textures, and new ones
        // deserve a fresh attempt at handing them over.
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        self.cannot_share.set(false);
        self.size.set(size);
    }

    fn present(&self) {
        // Nothing to swap: the page draws into one surface and is read from the
        // same one, which is what `read_to_image` is documented to return.
    }

    fn make_current(&self) -> Result<(), Error> {
        self.device
            .borrow()
            .make_context_current(&self.context.borrow())
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.gleam_gl.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.glow_gl.clone()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, Size2D<i32, euclid::UnknownUnit>)> {
        let device = self.device.borrow();
        let mut context = self.context.borrow_mut();
        let size = device.surface_info(&surface).size;
        let texture = device.create_surface_texture(&mut context, surface).ok()?;
        let name = device
            .surface_texture_object(&texture)
            .map_or(0, |object| object.0.get());
        Some((texture, name, size))
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        let device = self.device.borrow();
        let mut context = self.context.borrow_mut();
        device
            .destroy_surface_texture(&mut context, surface_texture)
            .map_err(|(error, _)| error)
            .ok()
    }

    fn connection(&self) -> Option<Connection> {
        Some(self.device.borrow().connection())
    }
}

/// Empties the driver's error queue, so what is asked next is answered about
/// itself. Bounded, because a driver in a bad way can answer forever.
fn drain_errors(gl: &Rc<dyn gleam::gl::Gl>) {
    for _ in 0..16 {
        if gl.get_error() == gleam::gl::NO_ERROR {
            return;
        }
    }
}

/// The rows the other way up, and the colours put right in the same pass if the
/// driver would not read them in the window's order.
fn turned_over(
    rows: Vec<u8>,
    width: usize,
    height: usize,
    swap_red_and_blue: bool,
) -> Option<RgbaImage> {
    let stride = width * 4;
    if rows.len() < stride * height {
        return None;
    }
    let mut image = vec![0_u8; stride * height];
    for row in 0..height {
        let from = (height - 1 - row) * stride;
        let source = rows.get(from..from + stride)?;
        let target = image.get_mut(row * stride..(row + 1) * stride)?;
        target.copy_from_slice(source);
        if swap_red_and_blue {
            for pixel in target.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
    }
    RgbaImage::from_raw(width as u32, height as u32, image)
}

/// The rows the other way up: OpenGL hands back the bottom one first, and an
/// image starts at the top.
fn flipped(rows: Vec<u8>, width: usize, height: usize) -> Option<RgbaImage> {
    let stride = width * 4;
    if rows.len() < stride * height {
        return None;
    }
    let mut image = vec![0_u8; stride * height];
    for row in 0..height {
        let from = (height - 1 - row) * stride;
        image[row * stride..(row + 1) * stride].copy_from_slice(&rows[from..from + stride]);
    }
    RgbaImage::from_raw(width as u32, height as u32, image)
}

fn physical(size: PhysicalSize<u32>) -> Size2D<i32, euclid::UnknownUnit> {
    Size2D::new(size.width as i32, size.height as i32)
}

/// Linux and Windows each have their own way of handing the page's memory to the
/// window. Elsewhere there is nothing to allocate and nothing to lend, and these
/// stand in so the rest of this file does not have to ask which machine it is on.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod nothing {
    pub(crate) enum SharedBuffers {}
    pub(crate) enum SharedBuffer {}

    impl SharedBuffers {
        pub(crate) fn new(
            _address: &dyn Fn(&str) -> *const std::ffi::c_void,
            _gl: &std::rc::Rc<dyn gleam::gl::Gl>,
            _device: &surfman::Device,
        ) -> Option<Self> {
            None
        }

        pub(crate) fn allocate(&self, _width: u32, _height: u32) -> Option<SharedBuffer> {
            None
        }

        pub(crate) fn bind_to_texture(&self, _buffer: &SharedBuffer, _texture: u32) -> bool {
            false
        }

        pub(crate) fn discard(&self, _buffer: &SharedBuffer) {}
    }
}
