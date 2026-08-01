use std::ffi::{CString, c_void};
use std::os::fd::{FromRawFd as _, OwnedFd};

/// Handing a frame to the window without copying it means handing over the
/// buffer the graphics card drew into, as a descriptor. EGL knows how to produce
/// one from a texture, through an extension Mesa has carried for years, and the
/// entry points for it are fetched the same way every other OpenGL function is.
///
/// None of this is in the EGL headers a Rust crate would bind: the functions are
/// extensions, and their addresses come from the driver at run time.
pub(crate) struct DmaBufExporter {
    display: *mut c_void,
    context: *mut c_void,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    query_export: QueryExport,
    export: Export,
}

type CreateImage = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    u32,
    *mut c_void,
    *const i32,
) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type QueryExport =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut i32, *mut i32, *mut u64) -> u32;
type Export =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut i32, *mut i32, *mut i32) -> u32;

const EGL_GL_TEXTURE_2D: u32 = 0x30B1;
const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: u32 = 1;
/// Pixels row after row, nothing rearranged. The only arrangement the window's
/// own device will take without an extension it does not ask for.
const LINEAR: u64 = 0;

/// What a frame looks like once it has been handed over.
pub(crate) struct ExportedFrame {
    pub descriptor: OwnedFd,
    pub stride: u32,
    pub offset: u32,
    pub format: u32,
    pub modifier: u64,
}

impl DmaBufExporter {
    /// Looks the extension up. `address` is the driver's own function lookup --
    /// surfman hands one out for the context it made.
    pub(crate) fn new(address: impl Fn(&str) -> *const c_void) -> Option<Self> {
        let look_up = |name: &str| {
            let symbol = CString::new(name).ok()?;
            let pointer = address(symbol.to_str().ok()?);
            (!pointer.is_null()).then_some(pointer)
        };

        // The display and context this thread is currently drawing with; the
        // image has to be made against the same ones.
        type CurrentHandle = unsafe extern "C" fn() -> *mut c_void;
        #[allow(unsafe_code)]
        let (current_display, current_context): (CurrentHandle, CurrentHandle) = unsafe {
            (
                std::mem::transmute::<*const c_void, CurrentHandle>(look_up(
                    "eglGetCurrentDisplay",
                )?),
                std::mem::transmute::<*const c_void, CurrentHandle>(look_up(
                    "eglGetCurrentContext",
                )?),
            )
        };

        #[allow(unsafe_code)]
        let exporter = unsafe {
            Self {
                display: current_display(),
                context: current_context(),
                create_image: std::mem::transmute::<*const c_void, CreateImage>(look_up(
                    "eglCreateImageKHR",
                )?),
                destroy_image: std::mem::transmute::<*const c_void, DestroyImage>(look_up(
                    "eglDestroyImageKHR",
                )?),
                query_export: std::mem::transmute::<*const c_void, QueryExport>(look_up(
                    "eglExportDMABUFImageQueryMESA",
                )?),
                export: std::mem::transmute::<*const c_void, Export>(look_up(
                    "eglExportDMABUFImageMESA",
                )?),
            }
        };
        if exporter.display.is_null() || exporter.context.is_null() {
            log::warn!("no current display to share the page's frames through");
            return None;
        }
        Some(exporter)
    }

    /// Hands over the buffer behind an OpenGL texture. `None` when this driver
    /// arranges its pixels in a way the window cannot read, which is not an
    /// error -- the caller copies the frame instead.
    pub(crate) fn export(&self, texture: u32) -> Option<ExportedFrame> {
        #[allow(unsafe_code)]
        unsafe {
            let attributes = [EGL_NONE];
            let image = (self.create_image)(
                self.display,
                self.context,
                EGL_GL_TEXTURE_2D,
                texture as usize as *mut c_void,
                attributes.as_ptr(),
            );
            if image.is_null() {
                log::warn!("the page's texture cannot be shared: no image for it");
                return None;
            }

            let mut format = 0_i32;
            let mut planes = 0_i32;
            let mut modifier = 0_u64;
            let queried = (self.query_export)(
                self.display,
                image,
                &mut format,
                &mut planes,
                &mut modifier,
            );
            if queried != EGL_TRUE || planes != 1 {
                log::warn!("the page's frames come in {planes} planes, which cannot be shared");
                (self.destroy_image)(self.display, image);
                return None;
            }
            if modifier != LINEAR {
                log::info!(
                    "the page's frames are arranged as {modifier:#x}; the window can only take \
                     linear ones, so they will be copied instead"
                );
                (self.destroy_image)(self.display, image);
                return None;
            }

            let mut descriptor = -1_i32;
            let mut stride = 0_i32;
            let mut offset = 0_i32;
            let exported = (self.export)(
                self.display,
                image,
                &mut descriptor,
                &mut stride,
                &mut offset,
            );
            // The image has done its job either way: the descriptor outlives it.
            (self.destroy_image)(self.display, image);
            if exported != EGL_TRUE {
                log::warn!("the page's frames would not be handed over");
                // A driver may have written a descriptor before deciding it
                // could not go through with it.
                if descriptor >= 0 {
                    OwnedFd::from_raw_fd(descriptor);
                }
                return None;
            }
            if descriptor < 0 {
                log::warn!("the page's frames were handed over without a descriptor");
                return None;
            }

            Some(ExportedFrame {
                descriptor: OwnedFd::from_raw_fd(descriptor),
                stride: stride.max(0) as u32,
                offset: offset.max(0) as u32,
                format: format as u32,
                modifier,
            })
        }
    }
}
