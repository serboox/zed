use std::cell::Cell;
use std::ffi::{c_int, c_uint, c_void};

use surfman::Device;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIResource;
use windows::core::Interface as _;

/// Handing a frame to the window without copying it means the page and the window
/// must name the same memory. On Windows they already can: surfman builds its
/// OpenGL contexts on a Direct3D 11 device, and reflects Direct3D textures into
/// OpenGL through an extension the engine cannot start without in the first
/// place. So the page's faces are Direct3D textures, made shareable, and the
/// window opens the very same texture on its own device.
///
/// Which side may touch a texture is not a matter of care but of a lock: OpenGL
/// draws into it only while it holds it, and the window reads it only while it
/// does not.
pub(crate) struct SharedBuffers {
    /// The device the page's textures are made on -- surfman's own, because the
    /// extension will only reflect textures from the device it was opened with.
    /// This holds a reference of its own, since surfman's device is dropped
    /// before the textures are.
    device: ID3D11Device,
    /// surfman's bridge between the two APIs, from `wglDXOpenDeviceNV`. It
    /// belongs to surfman's device, which outlives every texture made here.
    interop: HANDLE,
    register: RegisterObject,
    unregister: UnregisterObject,
    lock: LockObjects,
    unlock: UnlockObjects,
}

type RegisterObject =
    unsafe extern "system" fn(HANDLE, *mut c_void, c_uint, c_uint, c_uint) -> HANDLE;
type UnregisterObject = unsafe extern "system" fn(HANDLE, HANDLE) -> c_int;
type LockObjects = unsafe extern "system" fn(HANDLE, c_int, *mut HANDLE) -> c_int;
type UnlockObjects = unsafe extern "system" fn(HANDLE, c_int, *mut HANDLE) -> c_int;

/// Red, green, blue and alpha, one byte each: what OpenGL writes, and what
/// `DXGI_FORMAT_R8G8B8A8_UNORM` holds. Naming it the way the other platform does
/// is what keeps the window from reading the page's colours back to front.
pub(crate) const FORMAT_ABGR8888: u32 = 0x3432_4241;

const GL_TEXTURE_2D: c_uint = 0x0DE1;
const WGL_ACCESS_READ_WRITE_NV: c_uint = 0x0001;

/// One texture, drawn into by the page and read by the window.
pub(crate) struct SharedBuffer {
    texture: ID3D11Texture2D,
    /// What the extension calls the pair of names for this memory. Locking and
    /// letting go both go through it, and it is what has to be broken when the
    /// face goes.
    object: Cell<HANDLE>,
    /// Whether OpenGL holds the texture now. The extension refuses to lock a
    /// texture twice or to unlock one that is not locked, so which side has it is
    /// remembered rather than asked.
    held_by_opengl: Cell<bool>,
    /// The texture as the window is given it. The handle belongs to the texture
    /// rather than to whoever is handed it, so nothing closes it.
    descriptor: isize,
    pub(crate) stride: u32,
    pub(crate) offset: u32,
    pub(crate) width: u32,
}

impl SharedBuffer {
    /// The texture under the name the window opens it by. Nothing is duplicated:
    /// the handle belongs to the texture, which outlives every frame made of it.
    ///
    /// `None` while OpenGL still holds the texture. Reading a texture the other
    /// API holds is not merely wrong but undefined, and may take the editor down
    /// with it, so this is refused here as well as ordered elsewhere: the caller
    /// goes back to copying frames, which is slower and always correct.
    pub(crate) fn share(&self) -> Option<gpui::SharedFrameHandle> {
        if self.held_by_opengl.get() {
            log::warn!("the page's texture is still being drawn into, so it is not lent");
            return None;
        }
        Some(self.descriptor)
    }
}

impl SharedBuffers {
    /// Looks up the extension surfman is already using, and takes a reference to
    /// the device it opened. `address` is the driver's own function lookup, which
    /// answers for the context that is current -- surfman's, by the time this is
    /// called.
    pub(crate) fn new(
        address: &dyn Fn(&str) -> *const c_void,
        _gl: &std::rc::Rc<dyn gleam::gl::Gl>,
        device: &Device,
    ) -> Option<Self> {
        let look_up = |name: &str| {
            let pointer = address(name);
            (!pointer.is_null()).then_some(pointer)
        };

        // Casting a function's address to the function it is: the only way to
        // reach an extension the driver hands out at run time.
        #[allow(unsafe_code)]
        let (register, unregister, lock, unlock) = unsafe {
            (
                std::mem::transmute::<*const c_void, RegisterObject>(look_up(
                    "wglDXRegisterObjectNV",
                )?),
                std::mem::transmute::<*const c_void, UnregisterObject>(look_up(
                    "wglDXUnregisterObjectNV",
                )?),
                std::mem::transmute::<*const c_void, LockObjects>(look_up("wglDXLockObjectsNV")?),
                std::mem::transmute::<*const c_void, UnlockObjects>(look_up(
                    "wglDXUnlockObjectsNV",
                )?),
            )
        };

        // Asked for last, because it raises the device's reference count and
        // there is nothing to give it back on the way out of here.
        let native = device.native_device();
        let raw_device = native.d3d11_device.cast::<c_void>();
        let interop = HANDLE(native.gl_dx_interop_device.cast::<c_void>());
        if raw_device.is_null() || interop.0.is_null() {
            return None;
        }
        #[allow(unsafe_code)]
        let d3d11_device = unsafe { ID3D11Device::from_raw(raw_device) };

        log::info!("the page's frames are drawn into shareable Direct3D textures");
        Some(Self {
            device: d3d11_device,
            interop,
            register,
            unregister,
            lock,
            unlock,
        })
    }

    /// A texture of `width` by `height` that both APIs can reach.
    pub(crate) fn allocate(&self, width: u32, height: u32) -> Option<SharedBuffer> {
        if width == 0 || height == 0 {
            return None;
        }
        let description = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            // The extension takes no usage other than the default one.
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        #[allow(unsafe_code)]
        let texture = unsafe {
            let mut made = None;
            self.device
                .CreateTexture2D(&description, None, Some(&mut made))
                .inspect_err(|error| {
                    log::info!("the page's frames cannot be made shareable: {error}")
                })
                .ok()?;
            made?
        };

        let resource = texture
            .cast::<IDXGIResource>()
            .inspect_err(|error| log::info!("the page's texture is not a resource: {error}"))
            .ok()?;
        #[allow(unsafe_code)]
        let handle = unsafe { resource.GetSharedHandle() }
            .inspect_err(|error| log::info!("the page's texture would not be lent: {error}"))
            .ok()?;
        if handle.0.is_null() {
            return None;
        }

        Some(SharedBuffer {
            texture,
            object: Cell::new(HANDLE(std::ptr::null_mut())),
            held_by_opengl: Cell::new(false),
            descriptor: handle.0 as isize,
            // A Direct3D texture is sampled rather than walked, so the driver
            // keeps its own row length to itself and the window never asks for
            // it. What is published is the picture's own row.
            stride: width.saturating_mul(4),
            offset: 0,
            width,
        })
    }

    /// Makes the OpenGL texture named `texture` another name for this buffer's
    /// memory, and leaves OpenGL holding it so that the page can draw.
    pub(crate) fn bind_to_texture(&self, buffer: &SharedBuffer, texture: u32) -> bool {
        // `wglDXSetResourceShareHandleNV` is deliberately not called: the
        // extension says it has no effect for Direct3D 11 resources.
        #[allow(unsafe_code)]
        let object = unsafe {
            (self.register)(
                self.interop,
                buffer.texture.as_raw(),
                texture,
                GL_TEXTURE_2D,
                WGL_ACCESS_READ_WRITE_NV,
            )
        };
        if object.0.is_null() {
            log::info!(
                "the page's texture cannot be drawn into through OpenGL: {}",
                std::io::Error::last_os_error()
            );
            return false;
        }
        buffer.object.set(object);
        self.take_back(buffer)
    }

    /// Says OpenGL has finished with this texture, so that the window may read it.
    pub(crate) fn lend(&self, buffer: &SharedBuffer) {
        if !buffer.held_by_opengl.get() {
            return;
        }
        let mut object = buffer.object.get();
        if object.0.is_null() {
            return;
        }
        #[allow(unsafe_code)]
        let unlocked = unsafe { (self.unlock)(self.interop, 1, &mut object) };
        if unlocked == 0 {
            log::warn!(
                "the page's texture would not be handed to the window: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        buffer.held_by_opengl.set(false);
    }

    /// Takes the texture back for OpenGL, so that the page may draw into it again.
    pub(crate) fn take_back(&self, buffer: &SharedBuffer) -> bool {
        if buffer.held_by_opengl.get() {
            return true;
        }
        let mut object = buffer.object.get();
        if object.0.is_null() {
            return false;
        }
        #[allow(unsafe_code)]
        let locked = unsafe { (self.lock)(self.interop, 1, &mut object) };
        if locked == 0 {
            log::warn!(
                "the page cannot draw into its own texture: {}",
                std::io::Error::last_os_error()
            );
            return false;
        }
        buffer.held_by_opengl.set(true);
        true
    }

    /// Breaks the pair of names. The texture itself goes when the buffer holding
    /// it does, and the window's own reference to it keeps it alive for as long
    /// as the window is still drawing it.
    pub(crate) fn discard(&self, buffer: &SharedBuffer) {
        self.lend(buffer);
        let object = buffer.object.replace(HANDLE(std::ptr::null_mut()));
        if object.0.is_null() {
            return;
        }
        #[allow(unsafe_code)]
        unsafe {
            (self.unregister)(self.interop, object);
        }
    }
}
