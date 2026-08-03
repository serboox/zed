use std::ffi::{c_int, c_void};
use std::rc::Rc;

use core_foundation::base::TCFType as _;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferRef, kCVPixelFormatType_32BGRA};
use core_video::r#return::kCVReturnSuccess;

/// Handing a frame to the window without copying it means the page and the
/// window must agree on the memory it lives in. On macOS that memory is an
/// `IOSurface`: the graphics card draws into one through OpenGL and reads one
/// through Metal, and each side names the same pixels.
///
/// So the page does not draw into an ordinary texture at all. It draws into one
/// of these, and the window is handed the very same surface.
///
/// There is nothing to keep hold of here -- every entry point is a framework
/// function and each surface belongs to the face that draws into it -- but the
/// type exists all the same, because whether this machine will do it at all is
/// worth settling once, before the page is committed to drawing this way.
pub(crate) struct SharedBuffers;

/// One surface, drawn into by the page and read by the window.
pub(crate) struct SharedBuffer {
    /// The surface itself. This is a reference of our own: the pixel buffer
    /// beside it holds another, and a frame the window still has holds a third,
    /// which is what lets the page give this one back while the window draws.
    surface: *mut c_void,
    /// The surface as the window's own renderer takes it. Made once, when the
    /// surface is: asking CoreVideo to wrap it again for every frame would be a
    /// piece of work per frame for an answer that never changes.
    pub(crate) image_buffer: CVPixelBuffer,
    /// Bytes from the start of one row of pixels to the start of the next, as
    /// the surface itself reports it. The allocator rounds this up, so it is
    /// usually longer than the pixels in a row.
    pub(crate) stride: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Pixels row after row, four bytes each, blue first. This is what OpenGL writes
/// through the binding below and what the window's own renderer reads, so both
/// sides are talking about the same bytes.
const BYTES_PER_PIXEL: i32 = 4;

/// What a surface has to be bound as. Rectangle textures are the only kind an
/// `IOSurface` can be bound to, so the page's own framebuffer is built round one
/// of these rather than the ordinary two-dimensional sort.
const TEXTURE_RECTANGLE: u32 = 0x84F5;
const GL_RGBA: u32 = 0x1908;
const GL_BGRA: u32 = 0x80E1;
const GL_UNSIGNED_INT_8_8_8_8_REV: u32 = 0x8367;
const NO_CGL_ERROR: c_int = 0;

impl SharedBuffers {
    /// Whether this machine will let the page draw where the window can read it.
    ///
    /// `_address` is the driver's own function lookup, which the Linux side of
    /// this needs and this one does not: everything here is a framework function
    /// with a name at link time. `gl` is the context the page draws with, and
    /// whether it will draw into one of these is settled by trying it.
    pub(crate) fn new(
        _address: &dyn Fn(&str) -> *const c_void,
        gl: &Rc<dyn gleam::gl::Gl>,
    ) -> Option<Self> {
        let buffers = Self;
        if !buffers.can_be_drawn_into(gl) {
            log::info!("this machine's surfaces are not ones the page can draw into");
            return None;
        }
        log::info!("the page's frames are held in surfaces the window reads as they lie");
        Some(buffers)
    }

    /// Whether a surface can be drawn into by the page's own context. Asked once,
    /// with a surface small enough not to matter, because the answer is about the
    /// pair of them and not about the size.
    fn can_be_drawn_into(&self, gl: &Rc<dyn gleam::gl::Gl>) -> bool {
        let Some(buffer) = self.allocate(64, 64) else {
            return false;
        };
        let textures = gl.gen_textures(1);
        let Some(&texture) = textures.first() else {
            self.discard(&buffer);
            return false;
        };
        gl.bind_texture(TEXTURE_RECTANGLE, texture);
        self.bind_to_texture(&buffer);
        let framebuffers = gl.gen_framebuffers(1);
        let complete = match framebuffers.first() {
            Some(&framebuffer) => {
                gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, framebuffer);
                gl.framebuffer_texture_2d(
                    gleam::gl::FRAMEBUFFER,
                    gleam::gl::COLOR_ATTACHMENT0,
                    TEXTURE_RECTANGLE,
                    texture,
                    0,
                );
                let status = gl.check_frame_buffer_status(gleam::gl::FRAMEBUFFER);
                gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, 0);
                gl.delete_framebuffers(&[framebuffer]);
                status == gleam::gl::FRAMEBUFFER_COMPLETE
            }
            None => false,
        };
        gl.delete_textures(&[texture]);
        // A driver in a bad way can answer with an error forever; this asks a
        // bounded number of times.
        for _ in 0..16 {
            if gl.get_error() == gleam::gl::NO_ERROR {
                break;
            }
        }
        self.discard(&buffer);
        complete
    }

    /// A surface of `width` by `height`, laid out row after row, wrapped so that
    /// the window's own renderer will take it.
    pub(crate) fn allocate(&self, width: u32, height: u32) -> Option<SharedBuffer> {
        if width == 0 || height == 0 {
            return None;
        }
        unsafe {
            let key = |name: CFStringRef| CFString::wrap_under_get_rule(name);
            let row = width as usize * BYTES_PER_PIXEL as usize;
            // A row the allocator would have to lengthen is a row neither side
            // agrees the length of, so it is asked for at the length it will
            // take anyway.
            let alignment = IOSurfaceGetPropertyAlignment(kIOSurfaceBytesPerRow).max(1);
            let properties = CFDictionary::from_CFType_pairs(&[
                (
                    key(kIOSurfaceWidth),
                    CFNumber::from(width as i32).as_CFType(),
                ),
                (
                    key(kIOSurfaceHeight),
                    CFNumber::from(height as i32).as_CFType(),
                ),
                (
                    key(kIOSurfaceBytesPerElement),
                    CFNumber::from(BYTES_PER_PIXEL).as_CFType(),
                ),
                (
                    key(kIOSurfaceBytesPerRow),
                    CFNumber::from(row.div_ceil(alignment).saturating_mul(alignment) as i64)
                        .as_CFType(),
                ),
                (
                    key(kIOSurfacePixelFormat),
                    CFNumber::from(kCVPixelFormatType_32BGRA as i32).as_CFType(),
                ),
            ]);

            let surface = IOSurfaceCreate(properties.as_concrete_TypeRef());
            if surface.is_null() {
                log::warn!("the page's surface would not be allocated at {width}x{height}");
                return None;
            }

            let mut wrapped: CVPixelBufferRef = std::ptr::null_mut();
            let status = core_video::pixel_buffer_io_surface::CVPixelBufferCreateWithIOSurface(
                std::ptr::null(),
                surface as *const _,
                std::ptr::null(),
                &mut wrapped,
            );
            if status != kCVReturnSuccess || wrapped.is_null() {
                log::warn!("the page's surface cannot be lent: CoreVideo said {status}");
                CFRelease(surface);
                return None;
            }

            Some(SharedBuffer {
                surface,
                image_buffer: CVPixelBuffer::wrap_under_create_rule(wrapped),
                // The surface is the authority on where its rows start, not the
                // length that was asked for.
                stride: IOSurfaceGetBytesPerRow(surface) as u32,
                width,
                height,
            })
        }
    }

    /// Makes the texture currently bound to `TEXTURE_RECTANGLE` this surface's
    /// memory.
    pub(crate) fn bind_to_texture(&self, buffer: &SharedBuffer) {
        unsafe {
            let context = CGLGetCurrentContext();
            let bound = CGLTexImageIOSurface2D(
                context,
                TEXTURE_RECTANGLE,
                GL_RGBA,
                buffer.width as i32,
                buffer.height as i32,
                GL_BGRA,
                GL_UNSIGNED_INT_8_8_8_8_REV,
                buffer.surface,
                0,
            );
            if bound != NO_CGL_ERROR {
                // The texture is left as it was, so the framebuffer built round
                // it will not be complete and the caller falls back to a texture
                // of its own.
                log::warn!("the page's surface would not become a texture: CGL error {bound}");
            }
        }
    }

    /// Gives back the reference this side holds. Whoever else holds one -- the
    /// pixel buffer, a frame the window has not finished with -- keeps the
    /// surface alive until they are done with it.
    pub(crate) fn discard(&self, buffer: &SharedBuffer) {
        unsafe {
            CFRelease(buffer.surface);
        }
    }

    /// What the page's textures have to be bound as on this machine.
    pub(crate) fn texture_target(&self) -> u32 {
        TEXTURE_RECTANGLE
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(reference: *const c_void);
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    static kIOSurfaceWidth: CFStringRef;
    static kIOSurfaceHeight: CFStringRef;
    static kIOSurfaceBytesPerElement: CFStringRef;
    static kIOSurfaceBytesPerRow: CFStringRef;
    static kIOSurfacePixelFormat: CFStringRef;

    fn IOSurfaceCreate(properties: CFDictionaryRef) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    fn IOSurfaceGetPropertyAlignment(property: CFStringRef) -> usize;
}

#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {
    fn CGLGetCurrentContext() -> *mut c_void;
    fn CGLTexImageIOSurface2D(
        context: *mut c_void,
        target: u32,
        internal_format: u32,
        width: i32,
        height: i32,
        format: u32,
        kind: u32,
        surface: *mut c_void,
        plane: u32,
    ) -> c_int;
}
