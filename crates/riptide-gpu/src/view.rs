//! Views and samplers: the records a title's descriptors hold.
//!
//! A descriptor is a few bytes at an address the title chooses, and the title may copy
//! those bytes about by itself. So a record says everything about its view in its own
//! bytes, and nothing about it is kept anywhere else: a copy of a record is the same view.
//!
//! Views for shaders take 32 bytes and samplers 16 (see `descriptor.rs`). Shader programs
//! read these records where they lie (decision D5, and T6 of the shader design), and the
//! title's compiled code takes numbers out of what it read: a texture's size, a buffer's
//! element count. So a record has the console GPU's own descriptor in the words code
//! reads, and in the words the hardware keeps to itself it has what the host needs: the
//! place of the texture's host view, or of the host sampler, in the heaps of
//! `bindless.rs`. The layouts are [`encode`]'s to say.

use crate::device::Device;
use crate::resource::Resource;
use crate::trace;

/// What a record is. Zero is memory nothing has been written to.
pub(crate) const KIND_CONSTANTS: u8 = 1;
pub(crate) const KIND_SHADER_RESOURCE: u8 = 2;
pub(crate) const KIND_UNORDERED_ACCESS: u8 = 3;
pub(crate) const KIND_RENDER_TARGET: u8 = 4;
pub(crate) const KIND_DEPTH_TARGET: u8 = 5;
pub(crate) const KIND_SAMPLER: u8 = 6;
/// Not a kind of record: what the first few placed views are shown under.
const KIND_PLACED: u8 = 7;

/// A view dimension of 1 is a buffer in every kind of view description but a depth
/// target's, which has no buffers.
const DIMENSION_BUFFER: u32 = 1;
/// `D3D12_BUFFER_SRV_FLAG_RAW` and `D3D12_BUFFER_UAV_FLAG_RAW`: untyped 4-byte words.
const FLAG_RAW: u32 = 1;
/// In a texture view: every level from the first, or every slice.
const ALL: u32 = u32::MAX;

/// A view for shaders: constants, a shader resource or unordered access.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct View {
    pub(crate) kind: u8,
    /// The view's dimension as the title gave it, or zero for "as the resource is".
    pub(crate) dimension: u8,
    /// Which plane of a texture with several (depth and stencil).
    pub(crate) plane: u8,
    /// Bit 0: a buffer of untyped words.
    pub(crate) flags: u8,
    pub(crate) format: u32,
    /// A buffer view's first byte, or the texture's first byte, as a title address.
    pub(crate) address: u64,
    /// A buffer view's element count (for constants, its size in bytes). A texture view's
    /// first level in bits 0 to 7, level count in bits 8 to 15 (255: all), and first
    /// slice in bits 16 to 31.
    pub(crate) extent: u32,
    /// A buffer view's element size in bytes. A texture view's slice count in bits 0 to 15
    /// (65535: all) and component mapping in bits 16 to 31.
    pub(crate) shape: u32,
    /// The resource object the view was made of, or zero. Only this crate follows it.
    pub(crate) resource: u64,
    /// A texture view's description as the title gave it, in the host's layout, or zeroes
    /// if it gave none.
    pub(crate) described: [u32; 10],
}

/// The word of a record that says what it is: the kind in its lowest byte, then the
/// dimension, the plane and the flags. It is the last word of a view's 32 bytes, which no
/// descriptor of the console's uses.
const TAG: usize = 7;

/// In a buffer's flags word: its bounds are counted in bytes, not elements.
const BOUNDS_IN_BYTES: u32 = 0x3000_0000;
/// In a buffer's flags word: it is a ray-tracing structure.
const STRUCTURE: u32 = 0x8000_0000;

/// The record of `view`, as eight words.
///
/// A buffer, constants or a ray-tracing structure is the console's buffer descriptor in
/// its first four words:
///
/// | Word | Holds |
/// | --- | --- |
/// | 0 | the low half of its first byte's title address |
/// | 1 | bits 0 to 7: the rest of the address. Bits 16 to 29: the element size in bytes, zero for untyped words |
/// | 2 | how many elements, or bytes for untyped words |
/// | 3 | flags: [`BOUNDS_IN_BYTES`], [`STRUCTURE`] |
/// | 4 | the format |
/// | 7 | the tag |
///
/// A texture is the console's image descriptor in the words code reads:
///
/// | Word | Holds |
/// | --- | --- |
/// | 0 | nothing |
/// | 1 | bits 30 and 31: the low two bits of the width less one. Below them, the format |
/// | 2 | bits 0 to 13: the rest of the width less one. Bits 14 to 29: the height less one |
/// | 3 | bits 0 to 11: which channel each of the four comes from. Bits 16 to 19: the last level. Bits 28 to 31: the kind of texture (8 one dimension, 9 two, 10 three, 11 cube, 12 and 13 arrays of one and two, 14 and 15 multisampled and its array) |
/// | 4 | the depth less one, or the slices less one |
/// | 5 | the place of the host view in the shaders' heap |
/// | 6 | bit 0: the format is of whole numbers. Bit 1: they are signed |
/// | 7 | the tag |
///
/// The size is that of the view's first level, and the levels are counted from it, as a
/// shader sees them through the host view.
pub(crate) fn encode(view: &View) -> [u32; 8] {
    let mut words = [0u32; 8];
    words[TAG] = u32::from(view.kind)
        | (u32::from(view.dimension) << 8)
        | (u32::from(view.plane) << 16)
        | (u32::from(view.flags) << 24);
    let buffer = view.kind == KIND_CONSTANTS
        || u32::from(view.dimension) == DIMENSION_BUFFER
        || view.dimension == 11;
    if view.kind == 0 {
        return words;
    }
    if buffer {
        words[0] = view.address as u32;
        words[1] = ((view.address >> 32) as u32) & 0xff;
        words[4] = view.format;
        if view.dimension == 11 {
            words[3] = STRUCTURE;
        } else if view.kind == KIND_CONSTANTS {
            // Constants are read in rows of 16 bytes.
            words[1] |= 16 << 16;
            words[2] = view.extent.div_ceil(16);
        } else if view.flags & 1 != 0 {
            words[2] = view.extent.saturating_mul(view.shape.max(1));
            words[3] = BOUNDS_IN_BYTES;
        } else {
            words[1] |= (view.shape & 0x3fff) << 16;
            words[2] = view.extent;
        }
        return words;
    }
    // SAFETY: a view's resource is null or live when the view is made.
    let resource = unsafe { (view.resource as *const Resource).as_ref() };
    let unordered = view.kind == KIND_UNORDERED_ACCESS;
    let description = resource.map(Resource::description);
    let first_level = view.extent & 0xff;
    let levels = match ((view.extent >> 8) & 0xff, description) {
        (0xff, Some(d)) => u32::from(d.mip_levels).saturating_sub(first_level).max(1),
        (0xff, None) => 1,
        (levels, _) => levels.max(1),
    };
    let size = |whole: u64| ((whole >> first_level).max(1) - 1) as u32;
    let (width, height, depth, array) = description.map_or((0, 0, 0, 1), |d| {
        let three = d.dimension == 4;
        (
            size(d.width),
            size(u64::from(d.height)),
            if three {
                size(u64::from(d.depth_or_array_size))
            } else {
                0
            },
            if three {
                1
            } else {
                u32::from(d.depth_or_array_size).max(1)
            },
        )
    });
    let first_slice = view.extent >> 16;
    let slices = match view.shape & 0xffff {
        0xffff => array.saturating_sub(first_slice).max(1),
        slices => slices.max(1),
    };
    let kind = texture_kind(u32::from(view.dimension), description);
    words[1] = ((width & 3) << 30) | (view.format & 0x3fff_ffff);
    words[2] = ((width >> 2) & 0x3fff) | ((height & 0xffff) << 14);
    words[3] = channels(view.shape >> 16) | (((levels - 1) & 0xf) << 16) | (kind << 28);
    words[4] = match kind {
        10 => depth,
        11..=13 | 15 => slices - 1,
        _ => 0,
    };
    words[5] = crate::bindless::NOTHING + kind - 8;
    words[6] = whole_numbers(view.format);
    if let Some(texture) = resource.and_then(Resource::host) {
        let mut described = view.described;
        if described == [0; 10] {
            if let Some(d) = description {
                described = undescribed(d, unordered);
            }
        } else {
            described[0] = readable(described[0], u32::from(view.plane));
        }
        match crate::bindless::view((view.resource as usize, unordered, described), texture) {
            Some(place) => {
                words[5] = place;
                // What a shader reads through the view is what the title's memory holds.
                if let Some(resource) = resource {
                    crate::upload::watch(resource);
                }
            }
            None => trace!("a texture view has no place in the shaders' heap: {view:x?}"),
        }
    }
    words
}

/// The console's number for the kind of texture a view of host dimension `dimension`
/// shows: the dimension numbers are those of the host's shader resource views, which its
/// unordered access views share where they have them. Zero is "as the resource is".
fn texture_kind(dimension: u32, resource: Option<&crate::resource::Description>) -> u32 {
    match dimension {
        2 => 8,
        3 => 12,
        4 => 9,
        5 => 13,
        6 => 14,
        7 => 15,
        8 => 10,
        9 | 10 => 11,
        _ => match resource {
            Some(d) if d.dimension == 2 && d.depth_or_array_size > 1 => 12,
            Some(d) if d.dimension == 2 => 8,
            Some(d) if d.dimension == 4 => 10,
            Some(d) if d.sample_count > 1 => 14,
            Some(d) if d.depth_or_array_size > 1 => 13,
            _ => 9,
        },
    }
}

/// The console's channel selection for the host's component mapping: three bits for each
/// of the four channels, 0 and 1 for those numbers and 4 to 7 for the texture's own four.
/// The host's mapping has three bits each too: 0 to 3 the texture's four, 4 zero, 5 one.
/// A mapping of zero is none given, and means each channel is itself.
fn channels(mapping: u32) -> u32 {
    if mapping == 0 {
        return 0xfac;
    }
    (0..4)
        .map(|channel| {
            let console = match (mapping >> (3 * channel)) & 7 {
                4 => 0,
                5 => 1,
                own => 4 + (own & 3),
            };
            console << (3 * channel)
        })
        .sum()
}

/// Bit 0 if `format` holds whole numbers, and bit 1 as well if they are signed.
pub(crate) fn whole_numbers(format: u32) -> u32 {
    match format {
        3 | 7 | 12 | 17 | 22 | 25 | 30 | 36 | 42 | 47 | 50 | 57 | 62 => 1,
        4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => 3,
        _ => 0,
    }
}

/// The format a shader reads a texture of `format` through: a depth format's plane as a
/// colour, since the host shows depth to shaders only that way, and any other format as
/// the host has it.
pub(crate) fn readable(format: u32, plane: u32) -> u32 {
    let host = crate::target::host_format(format).map_or(format, |host| host.0 as u32);
    match (host, plane) {
        (40 | 39, _) => 41,
        (55 | 53, _) => 56,
        (45 | 44, 0) => 46,
        (45 | 44, _) => 47,
        (20 | 19, 0) => 21,
        (20 | 19, _) => 22,
        (other, _) => other,
    }
}

/// The host description of a view the title did not describe: the whole resource, as it
/// is. The host takes "no description" too, except for a depth texture, whose host texture
/// has no format of its own to read through; so one is always written.
fn undescribed(resource: &crate::resource::Description, unordered: bool) -> [u32; 10] {
    let mut words = [0u32; 10];
    words[0] = readable(resource.format, 0);
    let array = u32::from(resource.depth_or_array_size);
    let layered = array > 1;
    if unordered {
        // Format, dimension, then: level; or level, first slice, slices.
        words[1] = match (resource.dimension, layered) {
            (2, false) => 2,
            (2, true) => 3,
            (4, _) => 8,
            (_, false) => 4,
            (_, true) => 5,
        };
        if resource.dimension == 4 || layered {
            words[4] = if resource.dimension == 4 {
                u32::MAX
            } else {
                array
            };
        }
    } else {
        // Format, dimension, mapping and a word of padding, then from word 4: first level,
        // levels; and for an array the first slice and the slices. A multisampled array
        // has only its first slice and its slices.
        words[1] = match (resource.dimension, layered, resource.sample_count > 1) {
            (2, false, _) => 2,
            (2, true, _) => 3,
            (4, _, _) => 8,
            (_, false, true) => 6,
            (_, true, true) => 7,
            (_, false, false) => 4,
            (_, true, false) => 5,
        };
        words[2] = 0x1688;
        match words[1] {
            6 => {}
            7 => words[5] = array,
            _ => {
                words[5] = u32::MAX;
                if layered && resource.dimension != 4 {
                    words[7] = array;
                }
            }
        }
    }
    words
}

/// The view a record at `handle` was made from, as far as the record's tag says: its kind.
/// Zero if nothing was written there.
///
/// # Safety
/// `handle` is the address of a view descriptor.
#[allow(dead_code)] // the first user is a draw that checks what it is given
pub(crate) unsafe fn kind_at(handle: u64) -> u8 {
    // SAFETY: as this function's contract says.
    unsafe { (handle as *const [u32; 8]).read_unaligned()[TAG] as u8 }
}

/// A render or depth target: the resource and the title's description of the view, which
/// only commands read.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) kind: u8,
    /// Whether the title gave a description. Without one the view is the whole resource.
    pub(crate) described: bool,
    pub(crate) resource: u64,
    /// `D3D12_RENDER_TARGET_VIEW_DESC` or `D3D12_DEPTH_STENCIL_VIEW_DESC`.
    pub(crate) description: [u32; 6],
}

const _: () = assert!(size_of::<Target>() == 40);

/// A sampler's record: four words. The first is [`KIND_SAMPLER`]; the last is the place of
/// the host sampler in the shaders' sampler heap (`bindless.rs`), in its low 12 bits, where
/// the console's own descriptor keeps something no shader program reads. Equal samplers
/// have one place, so a copy of a record is the same sampler.
pub(crate) type SamplerRecord = [u32; 4];

/// `D3D12_SAMPLER_DESC`, as 13 words.
pub(crate) type Sampler = [u32; 13];

/// The record of a sampler the host describes as `sampler`.
fn sampler_record(sampler: &Sampler) -> SamplerRecord {
    [
        u32::from(KIND_SAMPLER),
        0,
        0,
        crate::bindless::sampler(sampler),
    ]
}

/// The bytes one element of `format` takes in a buffer, for the formats buffers are made
/// of. `None` for block-compressed, video and unknown formats.
pub(crate) fn element_bytes(format: u32) -> Option<u32> {
    Some(match format {
        1..=4 => 16,
        5..=8 => 12,
        9..=22 => 8,
        23..=47 | 67 | 87..=93 => 4,
        48..=59 | 85 | 86 | 115 => 2,
        60..=65 => 1,
        _ => return None,
    })
}

/// Writes `record` at the descriptor `handle`, which is an address.
///
/// # Safety
/// `handle` is zero or the address of a descriptor of at least `T`'s size.
unsafe fn put<T>(handle: u64, record: T) {
    if handle != 0 {
        // SAFETY: as this function's contract says.
        unsafe { (handle as *mut T).write_unaligned(record) };
    }
}

/// Says what was made, for the first few of each kind: a title makes a great many.
fn show(kind: u8, line: std::fmt::Arguments<'_>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SHOWN: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
    if SHOWN[usize::from(kind) & 7].fetch_add(1, Ordering::Relaxed) < 6 {
        trace!("{line}");
    }
}

/// The title address and description of the resource at `resource`, if there is one.
///
/// # Safety
/// `resource` is null or a live resource.
unsafe fn of(resource: *const Resource) -> Option<(u64, u32, u32)> {
    // SAFETY: as this function's contract says.
    let resource = unsafe { resource.as_ref() }?;
    let description = resource.description();
    Some((
        resource.address(),
        description.dimension,
        description.format,
    ))
}

/// A view of the buffer at `base`: `count` elements of `stride` bytes from element `first`.
#[allow(clippy::too_many_arguments)]
fn buffer_view(
    kind: u8,
    base: u64,
    format: u32,
    first: u64,
    count: u32,
    stride: u32,
    flags: u32,
    resource: u64,
) -> View {
    let raw = flags & FLAG_RAW != 0;
    let element = match stride {
        0 if raw => 4,
        0 => element_bytes(format).unwrap_or_else(|| {
            trace!("a buffer view of format {format} has no element size here; 4 bytes is assumed");
            4
        }),
        stride => stride,
    };
    View {
        kind,
        dimension: DIMENSION_BUFFER as u8,
        plane: 0,
        flags: u8::from(raw),
        format,
        address: base.wrapping_add(first.wrapping_mul(u64::from(element))),
        extent: count,
        shape: element,
        resource,
        described: [0; 10],
    }
}

/// A view of a texture: `levels` levels from `first_level`, `slices` slices from
/// `first_slice`.
#[allow(clippy::too_many_arguments)]
fn texture_view(
    kind: u8,
    base: u64,
    format: u32,
    dimension: u32,
    first_level: u32,
    levels: u32,
    first_slice: u32,
    slices: u32,
    plane: u32,
    mapping: u32,
    resource: u64,
    described: &[u32; 10],
) -> View {
    View {
        kind,
        dimension: dimension as u8,
        plane: plane as u8,
        flags: 0,
        format,
        address: base,
        extent: (first_level & 0xff) | (levels.min(0xff) << 8) | ((first_slice & 0xffff) << 16),
        shape: slices.min(0xffff) | ((mapping & 0xffff) << 16),
        resource,
        described: *described,
    }
}

/// `ID3D12Device::CreateConstantBufferView`: `(description, descriptor)`. The description
/// is an address and a size.
///
/// # Safety
/// `description` is null or valid; `handle` is zero or a view descriptor's address.
pub(crate) unsafe extern "system" fn create_constants(
    _device: &Device,
    description: *const [u32; 4],
    handle: u64,
) {
    // SAFETY: the title's description; without one the view is of nothing.
    let view = match unsafe { description.as_ref() } {
        Some(&[low, high, bytes, _]) => View {
            kind: KIND_CONSTANTS,
            dimension: DIMENSION_BUFFER as u8,
            address: u64::from(low) | (u64::from(high) << 32),
            extent: bytes,
            shape: 1,
            ..View::default()
        },
        None => View {
            kind: KIND_CONSTANTS,
            ..View::default()
        },
    };
    show(
        KIND_CONSTANTS,
        format_args!("constants view at {handle:#x}: {view:x?}"),
    );
    // SAFETY: the title's descriptor.
    unsafe { put(handle, encode(&view)) };
}

/// `ID3D12Device::CreateShaderResourceView`: `(resource, description, descriptor)`.
///
/// # Safety
/// `resource` is null or a live resource; `description` is null or valid; `handle` is zero
/// or a view descriptor's address.
pub(crate) unsafe extern "system" fn create_shader_resource(
    _device: &Device,
    resource: *const Resource,
    description: *const [u32; 10],
    handle: u64,
) {
    // SAFETY: the title's resource.
    let (base, _, format) = unsafe { of(resource) }.unwrap_or((0, 0, 0));
    // SAFETY: the title's description.
    let view = shader_resource(base, format, resource as u64, unsafe {
        description.as_ref()
    });
    show(
        KIND_SHADER_RESOURCE,
        format_args!("shader resource view at {handle:#x}: {view:x?}"),
    );
    // SAFETY: the title's descriptor.
    unsafe { put(handle, encode(&view)) };
}

/// The shader resource view `description` asks for, of the resource `object` whose bytes
/// start at `base`.
fn shader_resource(
    base: u64,
    resource_format: u32,
    object: u64,
    description: Option<&[u32; 10]>,
) -> View {
    const KIND: u8 = KIND_SHADER_RESOURCE;
    match description {
        // Without a description the view is the whole resource, as it is.
        None => View {
            kind: KIND,
            dimension: 0,
            format: resource_format,
            address: base,
            extent: 0xff << 8,
            shape: 0xffff,
            resource: object,
            ..View::default()
        },
        Some(d) => {
            let format = if d[0] == 0 { resource_format } else { d[0] };
            let (dimension, mapping) = (d[1], d[2]);
            match dimension {
                DIMENSION_BUFFER => {
                    let first = u64::from(d[4]) | (u64::from(d[5]) << 32);
                    buffer_view(KIND, base, format, first, d[6], d[7], d[8], object)
                }
                // A ray-tracing structure: the description is its address.
                11 => View {
                    kind: KIND,
                    dimension: 11,
                    address: u64::from(d[4]) | (u64::from(d[5]) << 32),
                    resource: object,
                    ..View::default()
                },
                // Arrays, and cube arrays: levels, then first slice and slice count.
                3 | 5 | 10 => {
                    let plane = if dimension == 5 { d[8] } else { 0 };
                    texture_view(
                        KIND, base, format, dimension, d[4], d[5], d[6], d[7], plane, mapping,
                        object, d,
                    )
                }
                // Multisampled: no levels; the array has a first slice and a count.
                6 => texture_view(KIND, base, format, 6, 0, 1, 0, 1, 0, mapping, object, d),
                7 => texture_view(
                    KIND, base, format, 7, 0, 1, d[4], d[5], 0, mapping, object, d,
                ),
                // One texture: levels only, and a plane for two dimensions.
                _ => {
                    let plane = if dimension == 4 { d[6] } else { 0 };
                    texture_view(
                        KIND, base, format, dimension, d[4], d[5], 0, ALL, plane, mapping, object,
                        d,
                    )
                }
            }
        }
    }
}

/// The resource description and address a placed view names: `(resource description, view
/// description)`, where the view description is the usual ten words, then the address of
/// the memory, then four words that are not understood yet.
///
/// # Safety
/// Each pointer is null or valid.
unsafe fn placed(
    resource: *const crate::resource::Description,
    description: *const [u32; 16],
) -> Option<(u64, u32, u64, [u32; 10])> {
    // SAFETY: the title's descriptions.
    let (resource, words) = unsafe { (resource.as_ref()?, description.as_ref()?) };
    let address = u64::from(words[10]) | (u64::from(words[11]) << 32);
    if words[12..] != [0; 4] {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SHOWN: AtomicU32 = AtomicU32::new(0);
        if SHOWN.fetch_add(1, Ordering::Relaxed) < 4 {
            trace!(
                "a placed view at {address:#x} has words after its address: {:x?}",
                &words[12..]
            );
        }
    }
    let object = crate::resource::placed(address, resource);
    let mut view = [0u32; 10];
    view.copy_from_slice(&words[..10]);
    Some((address, resource.format, object as u64, view))
}

/// `ID3D12Device::CreatePlacedShaderResourceViewX`: `(resource description, view
/// description, descriptor)`. A view of memory the title placed itself, with no resource
/// object: the description says what lies at the address.
///
/// # Safety
/// Each description is null or valid; `handle` is zero or a view descriptor's address.
pub(crate) unsafe extern "system" fn create_placed_shader_resource(
    _device: &Device,
    resource: *const crate::resource::Description,
    description: *const [u32; 16],
    handle: u64,
) {
    // SAFETY: as received.
    let Some((address, format, object, words)) = (unsafe { placed(resource, description) }) else {
        return;
    };
    let view = shader_resource(address, format, object, Some(&words));
    show(
        KIND_PLACED,
        format_args!("placed shader resource view at {handle:#x}: {view:x?}"),
    );
    // SAFETY: the title's descriptor.
    unsafe { put(handle, encode(&view)) };
}

/// `ID3D12Device::CreatePlacedUnorderedAccessViewX`: as
/// [`create_placed_shader_resource`], for unordered access.
///
/// # Safety
/// As [`create_placed_shader_resource`].
pub(crate) unsafe extern "system" fn create_placed_unordered_access(
    _device: &Device,
    resource: *const crate::resource::Description,
    description: *const [u32; 16],
    handle: u64,
) {
    // SAFETY: as received.
    let Some((address, format, object, words)) = (unsafe { placed(resource, description) }) else {
        return;
    };
    let view = unordered_access(address, format, object, Some(&words));
    show(
        KIND_PLACED,
        format_args!("placed unordered access view at {handle:#x}: {view:x?}"),
    );
    // SAFETY: the title's descriptor.
    unsafe { put(handle, encode(&view)) };
}

/// `ID3D12Device::CreateUnorderedAccessView`: `(resource, counter resource, description,
/// descriptor)`.
///
/// # Safety
/// As [`create_shader_resource`]; `counter` is null or a live resource.
pub(crate) unsafe extern "system" fn create_unordered_access(
    _device: &Device,
    resource: *const Resource,
    counter: *const Resource,
    description: *const [u32; 10],
    handle: u64,
) {
    // SAFETY: the title's resource.
    let (base, _, format) = unsafe { of(resource) }.unwrap_or((0, 0, 0));
    if !counter.is_null() {
        trace!("an unordered access view at {handle:#x} has a counter, which is not kept");
    }
    // SAFETY: the title's description.
    let view = unordered_access(base, format, resource as u64, unsafe {
        description.as_ref()
    });
    show(
        KIND_UNORDERED_ACCESS,
        format_args!("unordered access view at {handle:#x}: {view:x?}"),
    );
    // SAFETY: the title's descriptor.
    unsafe { put(handle, encode(&view)) };
}

/// The unordered access view `description` asks for, of the resource `object` whose bytes
/// start at `base`.
fn unordered_access(
    base: u64,
    resource_format: u32,
    object: u64,
    description: Option<&[u32; 10]>,
) -> View {
    const KIND: u8 = KIND_UNORDERED_ACCESS;
    match description {
        None => View {
            kind: KIND,
            format: resource_format,
            address: base,
            extent: 0xff << 8,
            shape: 0xffff,
            resource: object,
            ..View::default()
        },
        Some(d) => {
            let format = if d[0] == 0 { resource_format } else { d[0] };
            match d[1] {
                DIMENSION_BUFFER => {
                    let first = u64::from(d[2]) | (u64::from(d[3]) << 32);
                    buffer_view(KIND, base, format, first, d[4], d[5], d[8], object)
                }
                // Arrays: one level, then first slice and slice count; two dimensions
                // have a plane after them.
                dimension @ (3 | 5) => {
                    let plane = if dimension == 5 { d[5] } else { 0 };
                    texture_view(
                        KIND, base, format, dimension, d[2], 1, d[3], d[4], plane, 0, object, d,
                    )
                }
                // Three dimensions: one level, then first depth slice and count.
                8 => texture_view(KIND, base, format, 8, d[2], 1, d[3], d[4], 0, 0, object, d),
                dimension => {
                    let plane = if dimension == 4 { d[3] } else { 0 };
                    texture_view(
                        KIND, base, format, dimension, d[2], 1, 0, ALL, plane, 0, object, d,
                    )
                }
            }
        }
    }
}

/// Writes a render or depth target record.
///
/// # Safety
/// `description` is null or valid; `handle` is zero or a target descriptor's address.
unsafe fn create_target(
    kind: u8,
    resource: *const Resource,
    description: *const [u32; 6],
    handle: u64,
) {
    // SAFETY: the title's description.
    let described = unsafe { description.as_ref() };
    let target = Target {
        kind,
        described: described.is_some(),
        resource: resource as u64,
        description: described.copied().unwrap_or_default(),
    };
    show(
        kind,
        format_args!("target view at {handle:#x}: {target:x?}"),
    );
    // SAFETY: the title's descriptor.
    unsafe { put(handle, target) };
}

/// `ID3D12Device::CreateRenderTargetView`: `(resource, description, descriptor)`.
///
/// # Safety
/// As [`create_target`].
pub(crate) unsafe extern "system" fn create_render_target(
    _device: &Device,
    resource: *const Resource,
    description: *const [u32; 6],
    handle: u64,
) {
    // SAFETY: as received.
    unsafe { create_target(KIND_RENDER_TARGET, resource, description, handle) };
}

/// `ID3D12Device::CreateDepthStencilView`: `(resource, description, descriptor)`.
///
/// # Safety
/// As [`create_target`].
pub(crate) unsafe extern "system" fn create_depth_target(
    _device: &Device,
    resource: *const Resource,
    description: *const [u32; 6],
    handle: u64,
) {
    // SAFETY: as received.
    unsafe { create_target(KIND_DEPTH_TARGET, resource, description, handle) };
}

/// The target record at `handle`, if one of `kind` was written there.
///
/// # Safety
/// `handle` is zero or a target descriptor's address.
#[allow(dead_code)] // the first user is setting render targets on a list
pub(crate) unsafe fn target(handle: u64, kind: u8) -> Option<Target> {
    if handle == 0 {
        return None;
    }
    // The kind is read alone first: memory nothing was written to has no valid `bool`.
    // SAFETY: as this function's contract says.
    if unsafe { (handle as *const u8).read() } != kind {
        return None;
    }
    // SAFETY: a record of that kind was written here whole.
    Some(unsafe { (handle as *const Target).read_unaligned() })
}

/// `ID3D12Device::CreateSampler`: `(description, descriptor)`.
///
/// # Safety
/// `description` is null or valid; `handle` is zero or a sampler descriptor's address.
pub(crate) unsafe extern "system" fn create_sampler(
    _device: &Device,
    description: *const Sampler,
    handle: u64,
) {
    // SAFETY: the title's description; without one nothing is made.
    let Some(description) = (unsafe { description.as_ref() }) else {
        return;
    };
    let record = sampler_record(description);
    show(
        KIND_SAMPLER,
        format_args!(
            "sampler at {handle:#x}: place {} in the shaders' heap, {description:x?}",
            record[3]
        ),
    );
    // SAFETY: the title's descriptor.
    unsafe { put(handle, record) };
}

/// The console's own sampler description, which names the GPU's sampler fields: 27 words.
/// Word 1 is the magnification filter and word 2 the minification filter (0 point, 1
/// linear, 2 and 3 the same with anisotropy), word 3 the filter between levels (0 none, 1
/// point, 2 linear), word 5 the way samples are combined (0 plain, 1 minimum, 2 maximum),
/// words 6 to 8 the address modes, words 9 to 11 the least and greatest level and the level
/// bias, word 13 the anisotropy, word 16 the comparison and words 17 to 20 the border
/// colour. The others tune the hardware and have nothing to map to.
pub(crate) type ConsoleSampler = [u32; 27];

/// The PC description of a console sampler.
pub(crate) fn from_console(words: &ConsoleSampler) -> Sampler {
    const ADDRESS_CLAMP: u32 = 3;
    const COMPARE_NEVER: u32 = 1;
    let linear = |filter: u32| u32::from(filter == 1 || filter == 3);
    let (magnify, minify, between, combine) = (words[1], words[2], words[3], words[5]);
    let anisotropic = magnify >= 2 || minify >= 2;
    // The console compares when the shader's instruction says so. The host compares only
    // with a comparison sampler, so a sampler with a real test (less to greater-or-equal)
    // is made one.
    let compares = (2..=7).contains(&words[16]);
    let reduction = match combine {
        1 => 2,
        2 => 3,
        _ => u32::from(compares),
    };
    let filter = if anisotropic {
        0x55
    } else {
        (linear(minify) << 4) | (linear(magnify) << 2) | u32::from(between == 2)
    };
    let finite = |word: u32, or: f32| {
        let value = f32::from_bits(word);
        if value.is_nan() {
            or
        } else {
            value.clamp(f32::MIN, f32::MAX)
        }
    };
    let address = |word: u32| {
        if (1..=5).contains(&word) {
            word
        } else {
            ADDRESS_CLAMP
        }
    };
    let least = finite(words[9], 0.0);
    // With no filter between levels the console samples the least level only.
    let greatest = if between == 0 {
        least
    } else {
        finite(words[10], least).max(least)
    };
    [
        (reduction << 7) | filter,
        address(words[6]),
        address(words[7]),
        address(words[8]),
        finite(words[11], 0.0).clamp(-16.0, 15.99).to_bits(),
        if anisotropic {
            words[13].clamp(1, 16)
        } else {
            1
        },
        if reduction == 1 {
            words[16]
        } else {
            COMPARE_NEVER
        },
        words[17],
        words[18],
        words[19],
        words[20],
        least.to_bits(),
        greatest.to_bits(),
    ]
}

/// `ID3D12Device::CreateSamplerX`: `(console description, descriptor)`.
///
/// # Safety
/// `description` is null or valid; `handle` is zero or a sampler descriptor's address.
pub(crate) unsafe extern "system" fn create_sampler_x(
    device: &Device,
    description: *const ConsoleSampler,
    handle: u64,
) {
    // SAFETY: the title's description; without one nothing is made.
    let Some(description) = (unsafe { description.as_ref() }) else {
        return;
    };
    let sampler = from_console(description);
    // SAFETY: a valid description and the title's descriptor.
    unsafe { create_sampler(device, &sampler, handle) };
}

/// The host sampler for a sampler as the console GPU itself holds it: four words, which a
/// program has as constants in its code when its root signature fixes the sampler.
///
/// | Word | Bits | Hold |
/// | --- | --- | --- |
/// | 0 | 0 to 8 | how each of three coordinates is addressed, three bits each |
/// | 0 | 9 to 11 | the anisotropy, as a power of two |
/// | 0 | 12 to 14 | the comparison: 0 never to 7 always, in the host's order |
/// | 0 | 29 and 30 | how samples are combined: 0 plain, 1 minimum, 2 maximum |
/// | 1 | 0 to 11, 12 to 23 | the least and the greatest level, in 256ths |
/// | 2 | 0 to 13 | the level bias, signed, in 256ths |
/// | 2 | 20 and 21, 22 and 23 | the magnification and minification filters: 0 point, 1 linear, 2 and 3 the same with anisotropy |
/// | 2 | 26 and 27 | the filter between levels: 0 none, 1 point, 2 linear |
/// | 3 | 30 and 31 | the border: 0 clear black, 1 black, 2 white, 3 a colour held elsewhere |
///
/// The layout is the public one of this GPU family's sampler registers. A border colour
/// held elsewhere cannot be followed from the four words, and is black here.
pub(crate) fn from_hardware(words: &[u32; 4]) -> Sampler {
    let field = |word: usize, first: u32, bits: u32| (words[word] >> first) & ((1 << bits) - 1);
    // Wrap, mirror, clamp, mirror once, then the same two with half a border texel (which
    // the host has not, and are the plain ones here), then border and mirror once.
    let address = |code: u32| match code {
        0 => 1,
        1 => 2,
        3 | 5 | 7 => 5,
        6 => 4,
        _ => 3,
    };
    let (magnify, minify, between) = (field(2, 20, 2), field(2, 22, 2), field(2, 26, 2));
    let anisotropic = magnify >= 2 || minify >= 2;
    let filter = if anisotropic {
        0x55
    } else {
        ((minify & 1) << 4) | ((magnify & 1) << 2) | u32::from(between == 2)
    };
    let reduction = match field(0, 29, 2) {
        1 => 2,
        2 => 3,
        _ => 0,
    };
    let least = field(1, 0, 12) as f32 / 256.0;
    // With no filter between levels the console samples the least level only.
    let greatest = if between == 0 {
        least
    } else {
        (field(1, 12, 12) as f32 / 256.0).max(least)
    };
    // Fourteen bits, signed.
    let bias = ((field(2, 0, 14) << 18) as i32 >> 18) as f32 / 256.0;
    let border: [f32; 4] = match field(3, 30, 2) {
        1 => [0.0, 0.0, 0.0, 1.0],
        2 => [1.0; 4],
        _ => [0.0; 4],
    };
    [
        (reduction << 7) | filter,
        address(field(0, 0, 3)),
        address(field(0, 3, 3)),
        address(field(0, 6, 3)),
        bias.to_bits(),
        if anisotropic {
            1 << field(0, 9, 3).min(4)
        } else {
            1
        },
        // The host counts comparisons from one.
        field(0, 12, 3) + 1,
        border[0].to_bits(),
        border[1].to_bits(),
        border[2].to_bits(),
        border[3].to_bits(),
        least.to_bits(),
        greatest.to_bits(),
    ]
}

/// `ID3D12Device::CopyDescriptorsSimple`: `(count, to, from, heap type)`. Records are
/// whole in their bytes, so a copy of the bytes is a copy of the views.
///
/// # Safety
/// `to` and `from` are zero or the addresses of `count` descriptors of that type.
pub(crate) unsafe extern "system" fn copy_simple(
    _device: &Device,
    count: u32,
    to: u64,
    from: u64,
    kind: u32,
) {
    let Some(step) = crate::descriptor::increment(kind) else {
        return;
    };
    if to == 0 || from == 0 {
        return;
    }
    let bytes = count as usize * step as usize;
    // SAFETY: both ranges are the title's descriptors; they may overlap.
    unsafe { std::ptr::copy(from as *const u8, to as *mut u8, bytes) };
}

/// `ID3D12Device::CopyDescriptors`: `(to range count, to starts, to sizes, from range
/// count, from starts, from sizes, heap type)`. The ranges on each side are taken one after
/// another; a null list of sizes means one descriptor per range.
///
/// # Safety
/// Each list is null or has as many entries as its count says, and each range is
/// descriptors of that type.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe extern "system" fn copy(
    _device: &Device,
    to_ranges: u32,
    to_starts: *const u64,
    to_sizes: *const u32,
    from_ranges: u32,
    from_starts: *const u64,
    from_sizes: *const u32,
    kind: u32,
) {
    let Some(step) = crate::descriptor::increment(kind).map(u64::from) else {
        return;
    };
    if to_starts.is_null() || from_starts.is_null() {
        return;
    }
    /// Every descriptor address of one side, in order.
    ///
    /// # Safety
    /// As [`copy`]'s contract says of one side's lists.
    unsafe fn each(ranges: u32, starts: *const u64, sizes: *const u32, step: u64) -> Vec<u64> {
        let mut all = Vec::new();
        for range in 0..ranges as usize {
            // SAFETY: the lists have `ranges` entries.
            let (start, size) = unsafe {
                (
                    starts.add(range).read_unaligned(),
                    if sizes.is_null() {
                        1
                    } else {
                        sizes.add(range).read_unaligned()
                    },
                )
            };
            all.extend((0..u64::from(size)).map(|index| start + index * step));
        }
        all
    }
    // SAFETY: as received.
    let (to, from) = unsafe {
        (
            each(to_ranges, to_starts, to_sizes, step),
            each(from_ranges, from_starts, from_sizes, step),
        )
    };
    for (&to, &from) in to.iter().zip(&from) {
        if to != 0 && from != 0 {
            // SAFETY: each is one descriptor of the title's; they may be the same one.
            unsafe { std::ptr::copy(from as *const u8, to as *mut u8, step as usize) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> &'static Device {
        crate::device::device()
    }

    /// Room for descriptors, at an address.
    fn room(words: &mut [u64]) -> u64 {
        words.as_mut_ptr() as u64
    }

    /// The record at an address.
    ///
    /// # Safety
    /// A view descriptor's worth of memory lies there.
    unsafe fn words(at: u64) -> [u32; 8] {
        // SAFETY: as this function's contract says.
        unsafe { (at as *const [u32; 8]).read_unaligned() }
    }

    #[test]
    fn a_buffer_record_is_the_consoles_buffer_descriptor() {
        let mut slots = [0u64; 12];
        let at = room(&mut slots);
        // Format unknown, buffer, mapping, pad, first element 3, 10 elements of 24 bytes.
        let structured = [0u32, 1, 0x1688, 0, 3, 0, 10, 24, 0, 0];
        // R32 typeless raw words from element 2.
        let raw = [39u32, 1, 0x1688, 0, 2, 0, 16, 0, 1, 0];
        // SAFETY: valid descriptions, and room for the records.
        unsafe {
            create_shader_resource(device(), std::ptr::null(), &structured, at);
            create_shader_resource(device(), std::ptr::null(), &raw, at + 32);
            let record = words(at);
            // The address of element 3, the element size, the count, counted in elements.
            assert_eq!(record[..4], [72, 24 << 16, 10, 0]);
            assert_eq!(kind_at(at), KIND_SHADER_RESOURCE);
            // Untyped words: no element size, and bounds in bytes.
            let record = words(at + 32);
            assert_eq!(record[..4], [8, 0, 64, BOUNDS_IN_BYTES]);
            assert_eq!(record[4], 39);
            // A ray-tracing structure is its address and a mark.
            let structure = [0u32, 11, 0x1688, 0, 0x4000, 0x12, 0, 0, 0, 0];
            create_shader_resource(device(), std::ptr::null(), &structure, at + 64);
            assert_eq!(words(at + 64)[..4], [0x4000, 0x12, 0, STRUCTURE]);
        }
    }

    #[test]
    fn a_texture_view_keeps_its_levels_slices_and_plane() {
        // A 2D array: format 28, mapping, first level 1, 4 levels, slice 2, 6 slices, plane 1.
        let array = [28u32, 5, 0x1688, 0, 1, 4, 2, 6, 1, 0];
        // One 2D texture, all levels.
        let whole = [28u32, 4, 0x1688, 0, 0, u32::MAX, 0, 0, 0, 0];
        let view = shader_resource(0, 28, 0, Some(&array));
        assert_eq!((view.dimension, view.plane, view.format), (5, 1, 28));
        assert_eq!(view.extent, 1 | (4 << 8) | (2 << 16));
        assert_eq!(view.shape, 6 | (0x1688 << 16));
        assert_eq!(view.described, array);
        let view = shader_resource(0, 28, 0, Some(&whole));
        assert_eq!(view.extent, 0xff << 8);
        assert_eq!(view.shape & 0xffff, 0xffff);
    }

    #[test]
    fn a_texture_record_has_the_numbers_code_reads_where_the_console_has_them() {
        // A view of no resource: an array of two dimensions, 4 levels, 6 slices.
        let array = [28u32, 5, 0x1688, 0, 1, 4, 2, 6, 0, 0];
        let record = encode(&shader_resource(0, 28, 0, Some(&array)));
        // Kind 13 (an array of two dimensions), last level 3, each channel itself.
        assert_eq!(record[3], 0xfac | (3 << 16) | (13 << 28));
        assert_eq!(record[4], 5);
        // With no texture behind it, the place is the view of nothing of its kind.
        assert_eq!(record[5], crate::bindless::NOTHING + 5);
        assert_eq!(
            record[TAG] & 0xffff,
            u32::from(KIND_SHADER_RESOURCE) | (5 << 8)
        );
        // A record nothing was written to is all zero, and so is not a view.
        assert_eq!(encode(&View::default()), [0; 8]);
    }

    #[test]
    fn a_textures_size_is_split_over_the_words_as_the_console_splits_it() {
        let texture = crate::resource::Description {
            dimension: 3,
            alignment: 0,
            width: 1920,
            height: 1080,
            depth_or_array_size: 1,
            mip_levels: 11,
            // A format the host has no texture for, so that no host texture is made.
            format: 190,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags: 0,
        };
        let object = crate::resource::placed(0x7_0000_0000, &texture) as u64;
        let record = encode(&shader_resource(0x7_0000_0000, 190, object, None));
        let width = (record[1] >> 30) | ((record[2] & 0x3fff) << 2);
        let height = (record[2] >> 14) & 0xffff;
        assert_eq!((width + 1, height + 1), (1920, 1080));
        assert_eq!((record[3] >> 16) & 0xf, 10);
        assert_eq!(record[3] >> 28, 9);
        // From its second level on, the size is that level's.
        let from_second = [190u32, 4, 0x1688, 0, 1, u32::MAX, 0, 0, 0, 0];
        let record = encode(&shader_resource(
            0x7_0000_0000,
            190,
            object,
            Some(&from_second),
        ));
        let width = (record[1] >> 30) | ((record[2] & 0x3fff) << 2);
        assert_eq!(width + 1, 960);
        assert_eq!((record[3] >> 16) & 0xf, 9);
    }

    #[test]
    fn channels_formats_and_undescribed_views_are_as_the_host_and_console_number_them() {
        // The host's "each channel itself" is the console's.
        assert_eq!(channels(0x1688), 0xfac);
        assert_eq!(channels(0), 0xfac);
        // Red everywhere but alpha, which is one.
        assert_eq!(
            channels(0x1000 | (5 << 9)),
            4 | (4 << 3) | (4 << 6) | (1 << 9)
        );
        assert_eq!(
            (whole_numbers(42), whole_numbers(43), whole_numbers(28)),
            (1, 3, 0)
        );
        // Depth is read as a colour, and stencil as the other plane.
        assert_eq!(
            (readable(40, 0), readable(45, 0), readable(45, 1)),
            (41, 46, 47)
        );
        assert_eq!(
            (readable(118, 0), readable(55, 0), readable(28, 0)),
            (46, 56, 28)
        );
        let depth = crate::resource::Description {
            dimension: 3,
            alignment: 0,
            width: 64,
            height: 64,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 40,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags: 2,
        };
        assert_eq!(
            undescribed(&depth, false)[..6],
            [41, 4, 0x1688, 0, 0, u32::MAX]
        );
        let layered = crate::resource::Description {
            depth_or_array_size: 6,
            format: 28,
            ..depth
        };
        assert_eq!(
            undescribed(&layered, false)[..8],
            [28, 5, 0x1688, 0, 0, u32::MAX, 0, 6]
        );
        assert_eq!(undescribed(&layered, true)[..5], [28, 5, 0, 0, 6]);
    }

    #[test]
    fn a_placed_view_is_of_the_memory_its_description_names() {
        let mut slots = [0u64; 12];
        let at = room(&mut slots);
        let texture = crate::resource::Description {
            dimension: 3,
            alignment: 0,
            width: 256,
            height: 128,
            depth_or_array_size: 1,
            mip_levels: 9,
            format: 190,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags: 0,
        };
        // A whole 2D texture at 0x1_2345_0000, format left to the resource.
        let mut description = [0u32; 16];
        description[..6].copy_from_slice(&[0, 4, 0x1688, 0, 0, u32::MAX]);
        (description[10], description[11]) = (0x2345_0000, 1);
        // SAFETY: valid descriptions, and room for the records.
        unsafe {
            create_placed_shader_resource(device(), &texture, &description, at);
            create_placed_shader_resource(device(), &texture, &description, at + 32);
            create_placed_shader_resource(device(), std::ptr::null(), &description, at + 64);
            let record = words(at);
            assert_eq!(kind_at(at), KIND_SHADER_RESOURCE);
            assert_eq!(record[1] & 0x3fff_ffff, 190);
            assert_eq!((record[2] >> 14) + 1, 128);
            // The same memory described the same way is the same record, and one resource.
            assert_eq!(words(at + 32), record);
            let (address, format, object, _) = placed(&texture, &description).unwrap();
            assert_eq!((address, format), (0x1_2345_0000, 190));
            let resource = &*(object as *const Resource);
            assert_eq!(resource.address(), 0x1_2345_0000);
            assert_eq!(resource.description(), &texture);
            // Without a resource description nothing is written.
            assert_eq!(slots[8..], [0; 4]);
        }
    }

    #[test]
    fn constants_are_an_address_and_rows_of_16_bytes() {
        let mut slots = [0u64; 4];
        let at = room(&mut slots);
        // SAFETY: a valid description, and room for the record.
        unsafe {
            create_constants(device(), &[0x2000, 0x1, 250, 0], at);
            assert_eq!(words(at)[..4], [0x2000, 1 | (16 << 16), 16, 0]);
            assert_eq!(kind_at(at), KIND_CONSTANTS);
            // A null descriptor is not written to.
            create_constants(device(), &[0, 0, 0, 0], 0);
        }
    }

    #[test]
    fn a_target_is_read_back_only_as_what_it_is() {
        let mut slots = [0u64; 12];
        let at = room(&mut slots);
        // SAFETY: valid descriptions, and room for the records.
        unsafe {
            assert_eq!(target(at, KIND_RENDER_TARGET), None);
            create_render_target(device(), std::ptr::null(), &[28, 4, 2, 0, 0, 0], at);
            create_depth_target(device(), std::ptr::null(), std::ptr::null(), at + 48);
            let color = target(at, KIND_RENDER_TARGET).unwrap();
            assert!(color.described);
            assert_eq!(color.description, [28, 4, 2, 0, 0, 0]);
            assert_eq!(target(at, KIND_DEPTH_TARGET), None);
            assert!(!target(at + 48, KIND_DEPTH_TARGET).unwrap().described);
            assert_eq!(target(0, KIND_RENDER_TARGET), None);
        }
    }

    #[test]
    fn equal_samplers_are_one_and_copies_are_the_same_sampler() {
        let mut slots = [0u64; 8];
        let at = room(&mut slots);
        let linear: Sampler = [0x15, 1, 1, 1, 0, 16, 0, 0, 0, 0, 0, 0, 0x7f7f_ffff];
        let point: Sampler = [0, 3, 3, 3, 0, 1, 0, 0, 0, 0, 0, 0, 0x7f7f_fffe];
        // SAFETY: valid descriptions, and room for the records.
        unsafe {
            create_sampler(device(), &linear, at);
            create_sampler(device(), &point, at + 16);
            create_sampler(device(), &linear, at + 32);
            let record = |at: u64| (at as *const SamplerRecord).read();
            assert_eq!(record(at), record(at + 32));
            assert_eq!(record(at)[0], u32::from(KIND_SAMPLER));
            // Where there is a host to make samplers, different ones have different places.
            if crate::host::host().is_ok() {
                assert_ne!(record(at)[3], record(at + 16)[3]);
                assert!(record(at)[3] < crate::bindless::COMPARING);
            }

            copy_simple(device(), 1, at + 48, at + 16, 1);
            assert_eq!(record(at + 48), record(at + 16));
        }
    }

    #[test]
    fn a_console_sampler_becomes_the_pc_sampler_that_samples_the_same() {
        let mut words = [0u32; 27];
        // Linear magnification, point minification, linear between levels, clamped.
        (words[1], words[2], words[3]) = (1, 0, 2);
        words[6..9].copy_from_slice(&[3, 3, 3]);
        words[10] = 1000.0f32.to_bits();
        let sampler = from_console(&words);
        assert_eq!(sampler[0], 0x5);
        assert_eq!(sampler[1..4], [3, 3, 3]);
        assert_eq!(f32::from_bits(sampler[12]), 1000.0);
        assert_eq!((sampler[5], sampler[6]), (1, 1));

        // A less-or-equal test makes a comparison sampler; "never" does not.
        words[16] = 4;
        assert_eq!(
            (from_console(&words)[0], from_console(&words)[6]),
            (0x85, 4)
        );
        words[16] = 1;
        assert_eq!(from_console(&words)[0], 0x5);

        // Anisotropy on either filter is anisotropy on all, at most 16.
        (words[2], words[13]) = (3, 64);
        assert_eq!(
            (from_console(&words)[0], from_console(&words)[5]),
            (0x55, 16)
        );

        // No filter between levels: the least level only. Unknown address modes clamp.
        let mut flat = [0u32; 27];
        (flat[9], flat[10], flat[6]) = (3.0f32.to_bits(), 8.0f32.to_bits(), 9);
        let sampler = from_console(&flat);
        assert_eq!(
            (f32::from_bits(sampler[11]), f32::from_bits(sampler[12])),
            (3.0, 3.0)
        );
        assert_eq!(sampler[1], 3);

        let mut slots = [0u64; 2];
        let at = room(&mut slots);
        // SAFETY: a valid description, and room for the record.
        unsafe {
            create_sampler_x(device(), &flat, at);
            let record = (at as *const SamplerRecord).read();
            assert_eq!(record, sampler_record(&sampler));
        }
    }

    #[test]
    fn a_sampler_as_the_gpu_holds_it_becomes_the_host_sampler_that_samples_the_same() {
        // Clamp, wrap, mirror; less-or-equal; linear both ways and between levels; levels
        // 0 to 12.5; a bias of minus a half.
        let words = [
            2 | (1 << 6) | (3 << 12),
            (0xc80) << 12,
            ((-128i32 as u32) & 0x3fff) | (1 << 20) | (1 << 22) | (2 << 26),
            2 << 30,
        ];
        let sampler = from_hardware(&words);
        assert_eq!(sampler[0], 0x15);
        assert_eq!(sampler[1..4], [3, 1, 2]);
        assert_eq!(f32::from_bits(sampler[4]), -0.5);
        assert_eq!((sampler[5], sampler[6]), (1, 4));
        assert_eq!(sampler[7..11], [1.0f32.to_bits(); 4]);
        assert_eq!(
            (f32::from_bits(sampler[11]), f32::from_bits(sampler[12])),
            (0.0, 12.5)
        );
        // Anisotropy of 2^3 on the minification filter, and the least of the samples.
        let sharp = from_hardware(&[(3 << 9) | (1 << 29), 0, 3 << 22, 0]);
        assert_eq!((sharp[0], sharp[5]), (0x100 | 0x55, 8));
        // No filter between levels: the least level only.
        let flat = from_hardware(&[0, 0x200 | (0xfff << 12), 0, 0]);
        assert_eq!(
            (f32::from_bits(flat[11]), f32::from_bits(flat[12])),
            (2.0, 2.0)
        );
    }

    #[test]
    fn ranges_are_copied_one_descriptor_after_another() {
        let mut from = [0u64; 16];
        let mut to = [0u64; 16];
        for (index, word) in from.iter_mut().enumerate() {
            *word = index as u64 + 1;
        }
        let (from_at, to_at) = (room(&mut from), room(&mut to));
        // Three descriptors from two ranges, into one range of two and one single.
        let from_starts = [from_at, from_at + 64];
        let from_sizes = [1u32, 2];
        let to_starts = [to_at + 32, to_at];
        let to_sizes = [2u32, 1];
        // SAFETY: lists as long as their counts, and ranges inside the arrays above.
        unsafe {
            copy(
                device(),
                2,
                to_starts.as_ptr(),
                to_sizes.as_ptr(),
                2,
                from_starts.as_ptr(),
                from_sizes.as_ptr(),
                0,
            );
        }
        // 32-byte descriptors are four words each.
        assert_eq!(to[4..8], from[0..4]);
        assert_eq!(to[8..12], from[8..12]);
        assert_eq!(to[0..4], from[12..16]);
        assert_eq!(to[12..16], [0; 4]);
    }

    #[test]
    fn formats_buffers_are_made_of_have_a_size() {
        assert_eq!(element_bytes(2), Some(16));
        assert_eq!(element_bytes(28), Some(4));
        assert_eq!(element_bytes(57), Some(2));
        assert_eq!(element_bytes(61), Some(1));
        assert_eq!(element_bytes(71), None);
    }
}
