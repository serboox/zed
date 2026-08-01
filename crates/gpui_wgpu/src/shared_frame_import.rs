use std::os::fd::AsRawFd as _;

use gpui::SharedFrame;
use wgpu::hal::api::Vulkan;

/// DRM's name for eight bits each of blue, green, red and alpha, in that byte
/// order. It is what an OpenGL surface written as BGRA arrives as, and what the
/// window's own textures already use.
const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
const DRM_FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
const DRM_FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
const DRM_FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
/// Pixels laid out row after row, with nothing rearranged for the graphics
/// card's convenience. The only arrangement that can be shared without an
/// extension the window's device does not ask for.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

fn texture_format(drm_format: u32) -> Option<wgpu::TextureFormat> {
    match drm_format {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Some(wgpu::TextureFormat::Bgra8Unorm),
        DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Some(wgpu::TextureFormat::Rgba8Unorm),
        _ => None,
    }
}

/// Wraps a frame another process drew as a texture this window can sample.
///
/// Nothing is copied: the buffer stays where the graphics card put it, and the
/// window is given a second name for it. Returns `None` when the frame cannot be
/// shared -- an arrangement this device cannot read, a format it does not know,
/// a driver without the extensions -- and the caller falls back to the slow path
/// of copying pixels through memory.
pub fn import_shared_frame(device: &wgpu::Device, frame: &SharedFrame) -> Option<wgpu::Texture> {
    if frame.modifier != DRM_FORMAT_MOD_LINEAR {
        log::warn!(
            "a shared frame arrived arranged as {:#x}, which this window cannot read",
            frame.modifier
        );
        return None;
    }
    let format = texture_format(frame.format)?;
    let size = wgpu::Extent3d {
        width: frame.width,
        height: frame.height,
        depth_or_array_layers: 1,
    };

    // Everything below hands raw handles between two graphics APIs, which is the
    // whole point of the exercise and cannot be expressed safely.
    #[allow(unsafe_code)]
    let hal_texture = unsafe {
        let hal_device = device.as_hal::<Vulkan>()?;
        import_into_vulkan(&hal_device, frame, format, size)?
    };

    #[allow(unsafe_code)]
    let texture = unsafe {
        device.create_texture_from_hal::<Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("shared frame"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
        )
    };
    Some(texture)
}

#[allow(unsafe_code)]
unsafe fn import_into_vulkan(
    device: &wgpu::hal::vulkan::Device,
    frame: &SharedFrame,
    format: wgpu::TextureFormat,
    size: wgpu::Extent3d,
) -> Option<wgpu::hal::vulkan::Texture> {
    use ash::vk;

    let raw = device.raw_device();
    let physical = device.raw_physical_device();
    let instance = device.shared_instance().raw_instance();

    let vulkan_format = match format {
        wgpu::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        _ => vk::Format::R8G8B8A8_UNORM,
    };

    unsafe {
        let mut shareable = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vulkan_format)
            .extent(vk::Extent3D {
                width: size.width,
                height: size.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            // Linear, because that is how the buffer was laid out; the graphics
            // card is told to read it as it is rather than as it would prefer.
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut shareable);
        let image = raw
            .create_image(&image_info, None)
            .inspect_err(|error| log::warn!("no image for the shared frame: {error}"))
            .ok()?;

        // The window's own device decides where each row starts in a linear
        // image, and it need not agree with whoever allocated the buffer. Since
        // there is no way to tell it otherwise -- that would take an extension
        // it does not ask for -- a buffer laid out differently is refused rather
        // than drawn skewed.
        let layout = raw.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                array_layer: 0,
            },
        );
        if layout.row_pitch != u64::from(frame.stride) || layout.offset != u64::from(frame.offset) {
            log::warn!(
                "a shared frame's rows are {} bytes apart from {}, and this window would \
                 have them {} apart from {}",
                frame.stride,
                frame.offset,
                layout.row_pitch,
                layout.offset
            );
            raw.destroy_image(image, None);
            return None;
        }

        let requirements = raw.get_image_memory_requirements(image);
        let external = ash::khr::external_memory_fd::Device::new(instance, raw);
        // The descriptor is duplicated: Vulkan takes ownership of what it is
        // given, and the frame keeps its own copy for as long as it lives.
        let descriptor = match nix_dup(frame.descriptor.as_raw_fd()) {
            Some(descriptor) => descriptor,
            None => {
                raw.destroy_image(image, None);
                return None;
            }
        };
        let mut properties = vk::MemoryFdPropertiesKHR::default();
        let asked = external.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            descriptor,
            &mut properties,
        );
        if let Err(error) = asked {
            log::warn!("the shared frame is not importable: {error}");
            libc_close(descriptor);
            raw.destroy_image(image, None);
            return None;
        }

        let memory_type = memory_type_index(
            instance,
            physical,
            requirements.memory_type_bits & properties.memory_type_bits,
        );
        let Some(memory_type) = memory_type else {
            log::warn!("no memory type on this device can hold the shared frame");
            libc_close(descriptor);
            raw.destroy_image(image, None);
            return None;
        };

        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(descriptor);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        let memory = match raw.allocate_memory(&allocate, None) {
            Ok(memory) => memory,
            Err(error) => {
                log::warn!("the shared frame's memory would not import: {error}");
                // Vulkan only takes the descriptor when the allocation succeeds.
                libc_close(descriptor);
                raw.destroy_image(image, None);
                return None;
            }
        };
        if let Err(error) = raw.bind_image_memory(image, memory, 0) {
            log::warn!("the shared frame would not bind to its memory: {error}");
            raw.free_memory(memory, None);
            raw.destroy_image(image, None);
            return None;
        }

        // What to undo when the window is done with this frame. wgpu calls it
        // when the texture is dropped.
        let raw_device = raw.clone();
        let drop_guard = Box::new(move || {
            raw_device.destroy_image(image, None);
            raw_device.free_memory(memory, None);
        });

        Some(device.texture_from_raw(
            image,
            &wgpu::hal::TextureDescriptor {
                label: Some("shared frame"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUses::RESOURCE,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            },
            Some(drop_guard),
            // The buffer belongs to whoever lent it; wgpu neither allocated it
            // nor may free it behind our backs.
            wgpu::hal::vulkan::TextureMemory::External,
        ))
    }
}

#[allow(unsafe_code)]
unsafe fn memory_type_index(
    instance: &ash::Instance,
    physical: ash::vk::PhysicalDevice,
    allowed: u32,
) -> Option<u32> {
    let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
    (0..memory.memory_type_count).find(|index| allowed & (1 << index) != 0)
}

/// A second descriptor for the same buffer.
#[allow(unsafe_code)]
fn nix_dup(descriptor: i32) -> Option<i32> {
    let copy = unsafe { libc::dup(descriptor) };
    (copy >= 0).then_some(copy)
}

#[allow(unsafe_code)]
fn libc_close(descriptor: i32) {
    unsafe {
        libc::close(descriptor);
    }
}
