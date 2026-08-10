use gpui::SharedFrame;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV,
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
};

/// DRM's names for eight bits each of the four channels, in the byte order they
/// appear in memory. A producer says which of them it wrote, whichever platform
/// it drew on, and this is where that is turned into what Direct3D calls it.
const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
const DRM_FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
const DRM_FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
const DRM_FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

fn texture_format(drm_format: u32) -> Option<DXGI_FORMAT> {
    match drm_format {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Some(DXGI_FORMAT_B8G8R8A8_UNORM),
        DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Some(DXGI_FORMAT_R8G8B8A8_UNORM),
        _ => None,
    }
}

/// A frame another part of the process drew, opened on this window's own device.
pub(crate) struct ImportedFrame {
    /// Kept for as long as the view is: dropping it lets go of the producer's
    /// texture.
    #[allow(dead_code)]
    pub(crate) texture: ID3D11Texture2D,
    pub(crate) view: ID3D11ShaderResourceView,
}

/// Opens a frame the page lent as a texture this window can sample.
///
/// Nothing is copied: the texture stays where the graphics card put it, and the
/// window is given a second name for it. Returns `None` when it cannot be
/// opened -- a producer on another adapter, a format this device does not know, a
/// texture whose size does not match what the frame claims -- and the caller
/// tells the producer, which goes back to handing over pixels.
pub(crate) fn import_shared_frame(
    device: &ID3D11Device,
    frame: &SharedFrame,
) -> Option<ImportedFrame> {
    let Some(format) = texture_format(frame.format) else {
        log::warn!(
            "a shared frame arrived as {:#x}, which this window cannot read",
            frame.format
        );
        return None;
    };
    if frame.descriptor == 0 {
        return None;
    }
    let handle = HANDLE(frame.descriptor as *mut std::ffi::c_void);

    // Opening a texture by a handle whose provenance this cannot check, and
    // trusting the driver's own description of it afterwards.
    #[allow(unsafe_code)]
    let texture = unsafe {
        let mut opened: Option<ID3D11Texture2D> = None;
        device
            .OpenSharedResource(handle, &mut opened)
            .inspect_err(|error| log::warn!("the page's texture would not open here: {error}"))
            .ok()?;
        opened?
    };

    // The window places the picture inside the texture from the numbers the frame
    // carries, so a texture that is not the size it claims would be drawn
    // stretched or cut. Refusing it is better than showing it wrong.
    #[allow(unsafe_code)]
    let description = unsafe {
        let mut description = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut description);
        description
    };
    let across = frame.buffer_width.max(frame.width);
    if description.Width != across || description.Height != frame.height {
        log::warn!(
            "a shared frame says it is {}x{} and the texture is {}x{}",
            across,
            frame.height,
            description.Width,
            description.Height
        );
        return None;
    }
    if description.Format != format {
        log::warn!(
            "a shared frame says it is {:#x} and the texture is {:?}",
            frame.format,
            description.Format
        );
        return None;
    }

    let view_description = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    #[allow(unsafe_code)]
    let view = unsafe {
        let mut view = None;
        device
            .CreateShaderResourceView(&texture, Some(&view_description), Some(&mut view))
            .inspect_err(|error| log::warn!("the page's texture cannot be sampled: {error}"))
            .ok()?;
        view?
    };

    Some(ImportedFrame { texture, view })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
        D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_RESOURCE_MISC_SHARED, D3D11_SDK_VERSION, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
        D3D11CreateDevice, ID3D11DeviceContext,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::IDXGIResource;
    use windows::core::Interface as _;

    const WIDTH: u32 = 100;
    const HEIGHT: u32 = 64;

    /// A device on whichever adapter this machine will give. The runner that
    /// builds this has no graphics card, only the software rasterizer, and the
    /// whole arrangement has to hold there too.
    fn a_device() -> Option<(ID3D11Device, ID3D11DeviceContext, D3D_DRIVER_TYPE)> {
        for kind in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            let mut device = None;
            let mut context = None;
            #[allow(unsafe_code)]
            let made = unsafe {
                D3D11CreateDevice(
                    None,
                    kind,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG(0),
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            };
            if made.is_ok()
                && let (Some(device), Some(context)) = (device, context)
            {
                return Some((device, context, kind));
            }
        }
        None
    }

    /// A texture laid out the way the page's own faces are, written with a red half
    /// above a blue half so that which end of it arrives where can be told.
    fn a_lent_texture(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Option<(ID3D11Texture2D, isize)> {
        let description = D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut rows = vec![0_u8; (WIDTH * HEIGHT * 4) as usize];
        for row in 0..HEIGHT {
            let colour = if row < HEIGHT / 2 {
                [255_u8, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            for column in 0..WIDTH {
                let at = ((row * WIDTH + column) * 4) as usize;
                rows[at..at + 4].copy_from_slice(&colour);
            }
        }

        #[allow(unsafe_code)]
        let texture = unsafe {
            let mut made = None;
            device
                .CreateTexture2D(&description, None, Some(&mut made))
                .ok()?;
            let texture: ID3D11Texture2D = made?;
            context.UpdateSubresource(
                &texture,
                0,
                None,
                rows.as_ptr() as *const std::ffi::c_void,
                WIDTH * 4,
                0,
            );
            // One device is under no obligation to have finished before another
            // looks; the read below is waited for rather than assumed.
            context.Flush();
            texture
        };

        let resource = texture.cast::<IDXGIResource>().ok()?;
        #[allow(unsafe_code)]
        let handle = unsafe { resource.GetSharedHandle() }.ok()?;
        Some((texture, handle.0 as isize))
    }

    fn a_frame(descriptor: isize, format: u32, height: u32) -> SharedFrame {
        SharedFrame {
            descriptor,
            width: WIDTH,
            height,
            buffer_width: WIDTH,
            stride: WIDTH * 4,
            offset: 0,
            format,
            bottom_up: true,
            modifier: 0,
            refused: AtomicBool::new(false),
        }
    }

    /// Every row of a texture, as red, green and blue per pixel -- the order the
    /// texture itself holds them in -- through a copy the processor may read.
    fn read_back(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
    ) -> Option<Vec<Vec<[u8; 3]>>> {
        let description = D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        #[allow(unsafe_code)]
        unsafe {
            let mut staging = None;
            device
                .CreateTexture2D(&description, None, Some(&mut staging))
                .ok()?;
            let staging: ID3D11Texture2D = staging?;
            context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .ok()?;
            // A staging texture has a row length of the driver's choosing, which is
            // why the picture is read row by row rather than as one block.
            let bytes = std::slice::from_raw_parts(
                mapped.pData as *const u8,
                (mapped.RowPitch * HEIGHT) as usize,
            );
            let rows = (0..HEIGHT)
                .map(|row| {
                    (0..WIDTH)
                        .map(|column| {
                            let at = (row * mapped.RowPitch + column * 4) as usize;
                            [bytes[at], bytes[at + 1], bytes[at + 2]]
                        })
                        .collect()
                })
                .collect();
            context.Unmap(&staging, 0);
            Some(rows)
        }
    }

    /// The whole hand-over, across two devices: a texture one of them made and
    /// wrote, opened by the other from nothing but the frame's own numbers, and
    /// read back to see that the picture came through it.
    #[test]
    fn a_lent_texture_arrives_whole() {
        let Some((page_device, page_context, kind)) = a_device() else {
            println!("SHARED: this machine has no Direct3D 11 device at all");
            return;
        };
        let Some((window_device, window_context, _)) = a_device() else {
            println!("SHARED: this machine will give only one Direct3D 11 device");
            return;
        };
        println!("SHARED: two Direct3D 11 devices, driver type {kind:?}");

        let Some((texture, descriptor)) = a_lent_texture(&page_device, &page_context) else {
            println!("SHARED: this machine will not make a shareable texture");
            return;
        };

        let frame = a_frame(descriptor, DRM_FORMAT_ABGR8888, HEIGHT);
        // Named rather than skipped: if a machine cannot hand a texture from one
        // device to another, that is the answer, and it should be read as one.
        let imported = import_shared_frame(&window_device, &frame).unwrap_or_else(|| {
            panic!("a {kind:?} device would not open a texture another one lent it")
        });
        assert!(!frame.is_refused());

        // Waited for rather than assumed: what the other device wrote arrives when
        // it arrives, and a test that reads once is a test that reads too early.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut rows = read_back(&window_device, &window_context, &imported.texture)
            .expect("the imported texture should be readable through a copy");
        while Instant::now() < deadline && rows[8][32] != [255, 0, 0] {
            std::thread::sleep(Duration::from_millis(20));
            rows = read_back(&window_device, &window_context, &imported.texture)
                .expect("the imported texture should be readable through a copy");
        }
        assert_eq!(
            rows[8][32],
            [255, 0, 0],
            "the first rows of the lent texture should be its red half"
        );
        assert_eq!(
            rows[HEIGHT as usize - 8][32],
            [0, 0, 255],
            "the last rows of the lent texture should be its blue half"
        );

        // The importer trusts the driver's description over the frame's claims,
        // because the window places the picture from the frame's numbers alone.
        let taller = a_frame(descriptor, DRM_FORMAT_ABGR8888, HEIGHT + 1);
        assert!(
            import_shared_frame(&window_device, &taller).is_none(),
            "a frame claiming a size the texture does not have should be refused"
        );
        let unknown = a_frame(descriptor, fourcc(b'N', b'V', b'1', b'2'), HEIGHT);
        assert!(
            import_shared_frame(&window_device, &unknown).is_none(),
            "a frame in a layout this window cannot read should be refused"
        );
        assert!(
            import_shared_frame(&window_device, &a_frame(0, DRM_FORMAT_ABGR8888, HEIGHT)).is_none(),
            "a frame with no texture behind it should be refused"
        );

        drop(texture);
    }
}
