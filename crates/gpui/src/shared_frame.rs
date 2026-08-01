use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{DevicePixels, Size, size};

/// A frame that already lives in graphics memory, lent to the window rather than
/// copied into it.
///
/// Something else -- an embedded web engine, a video decoder -- drew this and
/// handed over the buffer it drew into, as a file descriptor the graphics driver
/// understands. The renderer wraps that descriptor as a texture and samples it
/// where it stands. Nothing is read back, nothing is uploaded.
///
/// The descriptor is closed when the last reference to this is dropped, so the
/// producer must keep the buffer alive for as long as it may still be drawn.
#[derive(Debug)]
pub struct SharedFrame {
    /// The buffer itself, as the graphics driver hands it out.
    pub descriptor: OwnedFd,
    /// How wide the frame is, in device pixels.
    pub width: u32,
    /// How tall the frame is, in device pixels.
    pub height: u32,
    /// Bytes from the start of one row of pixels to the start of the next.
    pub stride: u32,
    /// Where the first row starts within the buffer.
    pub offset: u32,
    /// The pixel layout, as a DRM fourcc code.
    pub format: u32,
    /// Whether the first row in the buffer is the bottom of the picture. A
    /// producer that draws with OpenGL hands over a buffer this way round, since
    /// OpenGL counts rows from the bottom and everyone reading a buffer starts
    /// at the top.
    pub bottom_up: bool,
    /// How the pixels are arranged in memory, as a DRM format modifier.
    /// Only a linear arrangement can be shared without an extension the window's
    /// own device does not ask for, so a producer should hand over linear
    /// buffers.
    pub modifier: u64,
    /// Set by the renderer when it turns out it cannot draw this buffer after
    /// all. A producer is expected to look, and to go back to handing over
    /// pixels; a frame that is refused is never drawn.
    pub refused: AtomicBool,
}

impl SharedFrame {
    /// Says this buffer cannot be drawn, so whoever produced it stops offering
    /// it and copies frames instead.
    pub fn refuse(&self) {
        self.refused.store(true, Ordering::Relaxed);
    }

    /// Whether the renderer has given up on this buffer.
    pub fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    /// How large the frame is, in device pixels.
    pub fn size(&self) -> Size<DevicePixels> {
        size(
            DevicePixels(self.width as i32),
            DevicePixels(self.height as i32),
        )
    }
}

impl PartialEq for SharedFrame {
    /// Two of these are the same frame when they are the same buffer: the
    /// descriptor is the identity here, not the numbers beside it.
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for SharedFrame {}
