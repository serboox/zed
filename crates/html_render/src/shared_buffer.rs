use std::ffi::{CString, c_int, c_uint, c_void};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::rc::Rc;

/// Handing a frame to the window without copying it means the page and the
/// window must agree on the memory it lives in. A texture the driver allocates
/// for itself is arranged for its own convenience -- Intel hands out a tiled
/// one -- and the window will not read that. A buffer asked for through the
/// driver's own allocator can be demanded row after row instead, and that one
/// both sides understand.
///
/// So the page does not draw into an ordinary texture at all. It draws into one
/// of these, and the window is handed the very same memory.
pub(crate) struct SharedBuffers {
    device: *mut c_void,
    display: *mut c_void,
    /// The render node the buffers come from. Closing it before the buffers are
    /// gone would pull the ground from under them.
    _card: OwnedFd,
    create_buffer: CreateWithModifiers,
    buffer_descriptor: BufferDescriptor,
    buffer_stride: BufferStride,
    buffer_offset: BufferOffset,
    destroy_buffer: DestroyBuffer,
    destroy_device: DestroyDevice,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_to_texture: ImageToTexture,
}

type CreateDevice = unsafe extern "C" fn(c_int) -> *mut c_void;
type DestroyDevice = unsafe extern "C" fn(*mut c_void);
type CreateWithModifiers =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, c_uint) -> *mut c_void;
type BufferDescriptor = unsafe extern "C" fn(*mut c_void) -> c_int;
type BufferStride = unsafe extern "C" fn(*mut c_void) -> u32;
type BufferOffset = unsafe extern "C" fn(*mut c_void, c_int) -> u32;
type DestroyBuffer = unsafe extern "C" fn(*mut c_void);
type CreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageToTexture = unsafe extern "C" fn(u32, *mut c_void);

/// Red, green, blue and alpha, one byte each. This is the order OpenGL writes
/// in, and saying so is what keeps the window from reading the page's colours
/// back to front.
pub(crate) const FORMAT_ABGR8888: u32 = 0x3432_4241;
/// Row after row, nothing rearranged.
const LINEAR: u64 = 0;
const EGL_NONE: i32 = 0x3038;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF: u32 = 0x3270;
const EGL_DRM_FOURCC: i32 = 0x3271;
const EGL_PLANE0_FD: i32 = 0x3272;
const EGL_PLANE0_OFFSET: i32 = 0x3273;
const EGL_PLANE0_PITCH: i32 = 0x3274;
const EGL_PLANE0_MODIFIER_LOW: i32 = 0x3443;
const EGL_PLANE0_MODIFIER_HIGH: i32 = 0x3444;

/// How many pixels wide a buffer is rounded up to. Four bytes each makes 256,
/// which is what the allocator rounds a row up to anyway -- asking for a width
/// it does not have to pad is what makes the window and the allocator agree on
/// where each row starts.
const WIDTH_STEP: u32 = 64;

/// One buffer, drawn into by the page and read by the window.
pub(crate) struct SharedBuffer {
    buffer: *mut c_void,
    image: *mut c_void,
    /// The buffer as the window is given it. It is duplicated for each frame
    /// handed over, so this one stays valid for as long as the page draws here.
    pub(crate) descriptor: OwnedFd,
    pub(crate) stride: u32,
    pub(crate) offset: u32,
    /// How wide the buffer really is. The page sits at the left of it.
    pub(crate) width: u32,
}

impl SharedBuffers {
    /// Finds an allocator whose buffers this OpenGL context can actually draw
    /// into. A machine with two graphics cards will happily allocate on one and
    /// refuse to draw on the other, so each render node is tried for real.
    ///
    /// `address` is the driver's own function lookup, and `gl` the context the
    /// page draws with.
    pub(crate) fn new(
        address: &dyn Fn(&str) -> *const c_void,
        gl: &Rc<dyn gleam::gl::Gl>,
    ) -> Option<Self> {
        let library = allocator_library()?;
        let symbol = |name: &str| {
            let name = CString::new(name).ok()?;
            #[allow(unsafe_code)]
            let pointer = unsafe { libc::dlsym(library, name.as_ptr()) };
            (!pointer.is_null()).then_some(pointer)
        };
        let look_up = |name: &str| {
            let pointer = address(name);
            (!pointer.is_null()).then_some(pointer)
        };

        #[allow(unsafe_code)]
        let (create_device, parts) = unsafe {
            (
                std::mem::transmute::<*mut c_void, CreateDevice>(symbol("gbm_create_device")?),
                Parts {
                    create_buffer: std::mem::transmute::<*mut c_void, CreateWithModifiers>(symbol(
                        "gbm_bo_create_with_modifiers",
                    )?),
                    buffer_descriptor: std::mem::transmute::<*mut c_void, BufferDescriptor>(
                        symbol("gbm_bo_get_fd")?,
                    ),
                    buffer_stride: std::mem::transmute::<*mut c_void, BufferStride>(symbol(
                        "gbm_bo_get_stride",
                    )?),
                    buffer_offset: std::mem::transmute::<*mut c_void, BufferOffset>(symbol(
                        "gbm_bo_get_offset",
                    )?),
                    destroy_buffer: std::mem::transmute::<*mut c_void, DestroyBuffer>(symbol(
                        "gbm_bo_destroy",
                    )?),
                    destroy_device: std::mem::transmute::<*mut c_void, DestroyDevice>(symbol(
                        "gbm_device_destroy",
                    )?),
                    create_image: std::mem::transmute::<*const c_void, CreateImage>(look_up(
                        "eglCreateImageKHR",
                    )?),
                    destroy_image: std::mem::transmute::<*const c_void, DestroyImage>(look_up(
                        "eglDestroyImageKHR",
                    )?),
                    image_to_texture: std::mem::transmute::<*const c_void, ImageToTexture>(
                        look_up("glEGLImageTargetTexture2DOES")?,
                    ),
                },
            )
        };

        type CurrentDisplay = unsafe extern "C" fn() -> *mut c_void;
        #[allow(unsafe_code)]
        let display = unsafe {
            std::mem::transmute::<*const c_void, CurrentDisplay>(look_up("eglGetCurrentDisplay")?)()
        };
        if display.is_null() {
            return None;
        }

        for node in 128..136 {
            let path = format!("/dev/dri/renderD{node}");
            let Ok(card) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            else {
                continue;
            };
            let card = OwnedFd::from(card);
            #[allow(unsafe_code)]
            let device = unsafe { create_device(std::os::fd::AsRawFd::as_raw_fd(&card)) };
            if device.is_null() {
                continue;
            }
            let buffers = Self {
                device,
                display,
                _card: card,
                create_buffer: parts.create_buffer,
                buffer_descriptor: parts.buffer_descriptor,
                buffer_stride: parts.buffer_stride,
                buffer_offset: parts.buffer_offset,
                destroy_buffer: parts.destroy_buffer,
                destroy_device: parts.destroy_device,
                create_image: parts.create_image,
                destroy_image: parts.destroy_image,
                image_to_texture: parts.image_to_texture,
            };
            if buffers.can_be_drawn_into(gl) {
                log::info!("the page's frames are allocated through {path} and shared as they are");
                return Some(buffers);
            }
        }
        log::info!("no allocator on this machine makes buffers the page can share");
        None
    }

    /// Whether a buffer from this allocator can be drawn into by the page's own
    /// context. Asked once, with a buffer small enough not to matter, because
    /// the answer is about the pair of them and not about the size.
    fn can_be_drawn_into(&self, gl: &Rc<dyn gleam::gl::Gl>) -> bool {
        let Some(buffer) = self.allocate(64, 64) else {
            return false;
        };
        let textures = gl.gen_textures(1);
        let Some(&texture) = textures.first() else {
            self.discard(&buffer);
            return false;
        };
        gl.bind_texture(gleam::gl::TEXTURE_2D, texture);
        #[allow(unsafe_code)]
        unsafe {
            (self.image_to_texture)(gleam::gl::TEXTURE_2D, buffer.image);
        }
        let framebuffers = gl.gen_framebuffers(1);
        let complete = match framebuffers.first() {
            Some(&framebuffer) => {
                gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, framebuffer);
                gl.framebuffer_texture_2d(
                    gleam::gl::FRAMEBUFFER,
                    gleam::gl::COLOR_ATTACHMENT0,
                    gleam::gl::TEXTURE_2D,
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

    /// A buffer of `width` by `height`, laid out row after row, with an image
    /// that can be bound to a texture.
    pub(crate) fn allocate(&self, width: u32, height: u32) -> Option<SharedBuffer> {
        if width == 0 || height == 0 {
            return None;
        }
        let width = width.div_ceil(WIDTH_STEP) * WIDTH_STEP;
        #[allow(unsafe_code)]
        unsafe {
            let buffer =
                (self.create_buffer)(self.device, width, height, FORMAT_ABGR8888, &LINEAR, 1);
            if buffer.is_null() {
                return None;
            }
            let raw = (self.buffer_descriptor)(buffer);
            if raw < 0 {
                (self.destroy_buffer)(buffer);
                return None;
            }
            let descriptor = OwnedFd::from_raw_fd(raw);
            let stride = (self.buffer_stride)(buffer);
            let offset = (self.buffer_offset)(buffer, 0);
            if stride != width * 4 || offset != 0 {
                // The whole point of the rounded-up width is that the allocator
                // has nothing left to pad. One that pads anyway lays its rows
                // out where the window will not look for them.
                log::info!(
                    "the allocator puts a {width}-pixel row in {stride} bytes from {offset}, \
                     which the window would not read; frames will be copied"
                );
                (self.destroy_buffer)(buffer);
                return None;
            }

            // Every value here is a 32-bit EGL integer: this is the older
            // entry point, not the one that takes machine-word attributes.
            let attributes = [
                EGL_WIDTH,
                width as i32,
                EGL_HEIGHT,
                height as i32,
                EGL_DRM_FOURCC,
                FORMAT_ABGR8888 as i32,
                EGL_PLANE0_FD,
                raw,
                EGL_PLANE0_OFFSET,
                offset as i32,
                EGL_PLANE0_PITCH,
                stride as i32,
                EGL_PLANE0_MODIFIER_LOW,
                (LINEAR & 0xffff_ffff) as i32,
                EGL_PLANE0_MODIFIER_HIGH,
                (LINEAR >> 32) as i32,
                EGL_NONE,
            ];
            let image = (self.create_image)(
                self.display,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF,
                std::ptr::null_mut(),
                attributes.as_ptr(),
            );
            if image.is_null() {
                (self.destroy_buffer)(buffer);
                return None;
            }
            Some(SharedBuffer {
                buffer,
                image,
                descriptor,
                stride,
                offset,
                width,
            })
        }
    }

    /// Makes the texture currently bound to `TEXTURE_2D` this buffer's memory.
    pub(crate) fn bind_to_texture(&self, buffer: &SharedBuffer) {
        #[allow(unsafe_code)]
        unsafe {
            (self.image_to_texture)(gleam::gl::TEXTURE_2D, buffer.image);
        }
    }

    pub(crate) fn discard(&self, buffer: &SharedBuffer) {
        #[allow(unsafe_code)]
        unsafe {
            (self.destroy_image)(self.display, buffer.image);
            (self.destroy_buffer)(buffer.buffer);
        }
    }

    /// What the page's textures have to be bound as on this machine. An EGL
    /// image goes on an ordinary two-dimensional texture.
    pub(crate) fn texture_target(&self) -> u32 {
        gleam::gl::TEXTURE_2D
    }
}

impl Drop for SharedBuffers {
    fn drop(&mut self) {
        #[allow(unsafe_code)]
        unsafe {
            (self.destroy_device)(self.device);
        }
    }
}

/// The entry points, gathered before there is anything to put them in.
struct Parts {
    create_buffer: CreateWithModifiers,
    buffer_descriptor: BufferDescriptor,
    buffer_stride: BufferStride,
    buffer_offset: BufferOffset,
    destroy_buffer: DestroyBuffer,
    destroy_device: DestroyDevice,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_to_texture: ImageToTexture,
}

/// The allocator library, opened once for the life of the process. Each preview
/// would otherwise take a reference of its own and never give it back.
fn allocator_library() -> Option<*mut c_void> {
    static LIBRARY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let address = *LIBRARY.get_or_init(|| {
        let Ok(name) = CString::new("libgbm.so.1") else {
            return 0;
        };
        // Mesa's own EGL already has this open on any machine where it is worth
        // asking, so this usually only takes a reference to what is there.
        #[allow(unsafe_code)]
        let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
        if !library.is_null() {
            return library as usize;
        }
        #[allow(unsafe_code)]
        let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW) };
        library as usize
    });
    (address != 0).then_some(address as *mut c_void)
}
