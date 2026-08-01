use std::cell::{Cell, RefCell};
use std::rc::Rc;

use std::sync::Arc;

use dpi::PhysicalSize;
use euclid::Size2D;
use gpui::SharedFrame;

use crate::dma_buf_export::DmaBufExporter;
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
    /// How a texture becomes something the window can sample. Absent when this
    /// driver has no way to hand a buffer over, and then frames are copied.
    exporter: Option<DmaBufExporter>,
    /// The frame currently lent to the window, if any.
    shared: RefCell<Option<Arc<SharedFrame>>>,
    /// Whether handing frames over has already been found impossible on this
    /// machine. Asking again every frame costs an image each time and the answer
    /// does not change until the page is resized.
    cannot_share: Cell<bool>,
}

/// A texture and the framebuffer that draws into it.
struct RenderTarget {
    texture: u32,
    framebuffer: u32,
}

impl RenderTarget {
    fn new(gl: &Rc<dyn gleam::gl::Gl>, size: PhysicalSize<u32>) -> Option<Self> {
        let textures = gl.gen_textures(1);
        let texture = *textures.first()?;
        gl.bind_texture(gleam::gl::TEXTURE_2D, texture);
        gl.tex_image_2d(
            gleam::gl::TEXTURE_2D,
            0,
            gleam::gl::RGBA8 as i32,
            size.width as i32,
            size.height as i32,
            0,
            gleam::gl::RGBA,
            gleam::gl::UNSIGNED_BYTE,
            None,
        );
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
        let framebuffer = *framebuffers.first()?;
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
            return None;
        }
        Some(Self {
            texture,
            framebuffer,
        })
    }

    fn discard(&self, gl: &Rc<dyn gleam::gl::Gl>) {
        gl.delete_framebuffers(&[self.framebuffer]);
        gl.delete_textures(&[self.texture]);
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
        let target = RenderTarget::new(&gleam_gl, size).ok_or(Error::Failed)?;
        let exporter = DmaBufExporter::new(|name| device.get_proc_address(&context, name));
        if exporter.is_none() {
            log::info!("this driver cannot hand frames over, so they will be copied");
        }

        Ok(Self {
            gleam_gl,
            glow_gl,
            device: RefCell::new(device),
            context: RefCell::new(context),
            size: Cell::new(size),
            target: RefCell::new(target),
            exporter,
            shared: RefCell::new(None),
            cannot_share: Cell::new(false),
        })
    }

    /// The frame the window may sample directly, if this driver allows it. The
    /// same one is handed out until the page is resized.
    pub(crate) fn shared_frame(&self) -> Option<Arc<SharedFrame>> {
        if let Some(shared) = self.shared.borrow().as_ref() {
            return Some(shared.clone());
        }
        if self.cannot_share.get() {
            return None;
        }
        let exported = match self.exporter.as_ref()?.export(self.target.borrow().texture) {
            Some(exported) => exported,
            None => {
                self.cannot_share.set(true);
                return None;
            }
        };
        let size = self.size.get();
        let frame = Arc::new(SharedFrame {
            descriptor: exported.descriptor,
            width: size.width,
            height: size.height,
            stride: exported.stride,
            offset: exported.offset,
            format: exported.format,
            modifier: exported.modifier,
        });
        *self.shared.borrow_mut() = Some(frame.clone());
        log::info!(
            "the page's frames are handed to the window as they are, {}x{}",
            size.width,
            size.height
        );
        Some(frame)
    }

    fn framebuffer(&self) -> u32 {
        self.target.borrow().framebuffer
    }

    /// Waits for the graphics card to finish what it was told to draw. The
    /// window reads this buffer with a different API, which knows nothing of
    /// OpenGL's queue, so the drawing has to be done before it looks.
    pub(crate) fn finish_drawing(&self) {
        self.gleam_gl.finish();
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
            Ok(()) => self.target.borrow().discard(&self.gleam_gl),
            Err(error) => log::warn!("the page's own context would not come back: {error:?}"),
        }
        device.destroy_context(&mut context).ok();
    }
}

impl RenderingContext for GpuSurface {
    fn prepare_for_rendering(&self) {
        self.gleam_gl
            .bind_framebuffer(gleam::gl::FRAMEBUFFER, self.framebuffer());
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        let width = source_rectangle.width();
        let height = source_rectangle.height();
        if width <= 0 || height <= 0 {
            return None;
        }
        // The same sequence Servo reads with, including the empty vertex array:
        // some drivers hand back nothing at all without it.
        self.gleam_gl
            .bind_framebuffer(gleam::gl::FRAMEBUFFER, self.framebuffer());
        self.gleam_gl.bind_vertex_array(0);
        let mut rows = self.gleam_gl.read_pixels(
            source_rectangle.min.x,
            source_rectangle.min.y,
            width,
            height,
            gleam::gl::RGBA,
            gleam::gl::UNSIGNED_BYTE,
        );
        let error = self.gleam_gl.get_error();
        if error != gleam::gl::NO_ERROR {
            log::warn!("the page's surface could not be read: GL error 0x{error:x}");
        }
        let stride = width as usize * 4;
        if rows.len() < stride * height as usize {
            return None;
        }
        // OpenGL hands back the bottom row first; an image starts at the top.
        for row in 0..(height as usize / 2) {
            let (top, bottom) = (row * stride, (height as usize - 1 - row) * stride);
            for byte in 0..stride {
                rows.swap(top + byte, bottom + byte);
            }
        }
        RgbaImage::from_raw(width as u32, height as u32, rows)
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
        let Some(target) = RenderTarget::new(&self.gleam_gl, size) else {
            return;
        };
        let previous = std::mem::replace(&mut *self.target.borrow_mut(), target);
        previous.discard(&self.gleam_gl);
        // What the window was lent belonged to the old texture, and a new one
        // deserves a fresh attempt at handing it over.
        self.shared.borrow_mut().take();
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

fn physical(size: PhysicalSize<u32>) -> Size2D<i32, euclid::UnknownUnit> {
    Size2D::new(size.width as i32, size.height as i32)
}
