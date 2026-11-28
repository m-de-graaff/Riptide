//! Heaps and resources: the memory graphics objects lie in.
//!
//! On the console the CPU and the GPU share one memory, so every heap type is ordinary
//! title memory with different rules about who may touch it.

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::device::Device;
use riptide_mem::{Access, Limits};

use crate::object::{self, Child, E_INVALIDARG, E_OUTOFMEMORY, Hresult, Iid, Object, S_OK, guid};
use crate::trace;

/// `ID3D12Resource`, `ID3D12Resource1` and `ID3D12Resource2`.
const IIDS: [Iid; 3] = [
    guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    ),
    guid(
        0x9d5e227a,
        0x4430,
        0x4161,
        [0x88, 0xb3, 0x3e, 0xca, 0x6b, 0xb1, 0x6e, 0x19],
    ),
    guid(
        0xbe36ec3b,
        0xea85,
        0x4aeb,
        [0xa4, 0x5a, 0xe9, 0xd7, 0x64, 0x04, 0xa4, 0x95],
    ),
];

/// `D3D12_RESOURCE_DESC`, as on PC. The newer `D3D12_RESOURCE_DESC1` starts the same.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Description {
    /// 1 buffer, 2 to 4 textures of one to three dimensions.
    pub(crate) dimension: u32,
    pub(crate) alignment: u64,
    /// A buffer's size in bytes, or a texture's width.
    pub(crate) width: u64,
    pub(crate) height: u32,
    pub(crate) depth_or_array_size: u16,
    pub(crate) mip_levels: u16,
    pub(crate) format: u32,
    pub(crate) sample_count: u32,
    pub(crate) sample_quality: u32,
    pub(crate) layout: u32,
    pub(crate) flags: u32,
}

const _: () = assert!(size_of::<Description>() == 56);

const DIMENSION_BUFFER: u32 = 1;

/// `D3D12_HEAP_PROPERTIES`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeapProperties {
    /// 1 default, 2 upload, 3 read-back, 4 custom.
    pub(crate) kind: u32,
    /// How the CPU reaches the heap's pages: 0 unknown, 1 not at all, 2 write-combined,
    /// 3 write-back (ordinary cached memory).
    pub(crate) cpu_pages: u32,
    /// 0 unknown, 1 pool L0 (system memory), 2 pool L1 (video memory).
    pub(crate) pool: u32,
    pub(crate) creation_nodes: u32,
    pub(crate) visible_nodes: u32,
}

const _: () = assert!(size_of::<HeapProperties>() == 20);

const HEAP_DEFAULT: u32 = 1;
const HEAP_UPLOAD: u32 = 2;
const HEAP_READBACK: u32 = 3;
const HEAP_CUSTOM: u32 = 4;
const CPU_PAGES_NOT_AVAILABLE: u32 = 1;
const CPU_PAGES_WRITE_BACK: u32 = 3;
const POOL_L0: u32 = 1;

/// What a standard heap type is when written out as a custom heap, on a machine whose CPU
/// and GPU share one cache-coherent memory: everything is in pool L0, default heaps are not
/// for the CPU, and upload and read-back heaps are ordinary cached memory.
pub(crate) fn as_custom(kind: u32, nodes: u32) -> HeapProperties {
    let nodes = nodes.max(1);
    let cpu_pages = match kind {
        HEAP_DEFAULT => CPU_PAGES_NOT_AVAILABLE,
        HEAP_UPLOAD | HEAP_READBACK => CPU_PAGES_WRITE_BACK,
        _ => 0,
    };
    HeapProperties {
        kind: HEAP_CUSTOM,
        cpu_pages,
        pool: if cpu_pages == 0 { 0 } else { POOL_L0 },
        creation_nodes: nodes,
        visible_nodes: nodes,
    }
}

/// `ID3D12Device::GetCustomHeapProperties`: `(node mask, heap type)`, with the result
/// returned through a pointer the caller passes.
///
/// A method that returns a structure takes that pointer as a hidden argument. The title
/// passes it before the object, not after (measured: the first argument is not the device).
///
/// # Safety
/// `out` is null or room for the result.
pub(crate) unsafe extern "system" fn custom_heap_properties(
    out: *mut HeapProperties,
    _this: *const Device,
    nodes: u32,
    kind: u32,
) -> *mut HeapProperties {
    let properties = as_custom(kind, nodes);
    trace!("GetCustomHeapProperties(type {kind}, nodes {nodes:#x}) = {properties:?}");
    // SAFETY: the caller's result pointer; null is tolerated.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = properties;
    }
    out
}

/// `D3D12_RESOURCE_ALLOCATION_INFO`: the memory a resource, or several placed one after
/// another, take.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocationInfo {
    bytes: u64,
    alignment: u64,
}

/// `D3D12_RESOURCE_ALLOCATION_INFO1`: where one of several resources goes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    offset: u64,
    alignment: u64,
    bytes: u64,
}

/// What the documented answer is for a description that cannot be sized.
const UNKNOWN: AllocationInfo = AllocationInfo {
    bytes: u64::MAX,
    alignment: 0,
};

const KIB_64: u64 = 64 << 10;
const MIB_4: u64 = 4 << 20;
const DIMENSION_TEXTURE_3D: u32 = 4;

/// The bytes one block of `format` takes and how many texels wide and high a block is:
/// 4 for block-compressed formats, else 1. PC's formats, and the console's own: 116 and 117
/// (10-bit float colour), 118 to 120 (16-bit depth with stencil, 3 bytes), 189 (signed
/// 10-bit colour), 190 (two 4-bit channels) and 191 (24 and 8 bits). `None` for video and
/// unknown formats.
pub(crate) fn block(format: u32) -> Option<(u64, u64)> {
    Some(match format {
        1..=4 => (16, 1),
        5..=8 => (12, 1),
        9..=22 => (8, 1),
        23..=47 | 67 | 87..=93 | 116 | 117 | 189 | 191 => (4, 1),
        118..=120 => (3, 1),
        48..=59 | 68 | 69 | 85 | 86 | 115 => (2, 1),
        60..=65 | 190 => (1, 1),
        70..=72 | 79..=81 => (8, 4),
        73..=78 | 82..=84 | 94..=99 => (16, 4),
        _ => return None,
    })
}

/// How many levels a texture has: what its description says, or with zero every level
/// down to one texel.
fn level_count(description: &Description) -> u32 {
    if description.mip_levels != 0 {
        return u32::from(description.mip_levels);
    }
    let depth = if description.dimension == DIMENSION_TEXTURE_3D {
        u64::from(description.depth_or_array_size)
    } else {
        1
    };
    let largest = description
        .width
        .max(u64::from(description.height))
        .max(depth)
        .max(1);
    64 - largest.leading_zeros()
}

/// A texture's size and alignment in the console GPU's own layout, as `riptide-formats`
/// lays it out: in the mode the description's layout field names (`0x100 | mode`), row
/// after row for 1, and for 0 the mode the console's library would choose for what the
/// texture is for. `None` for a description it cannot lay out.
///
/// The console also keeps compression data beside render targets and depth buffers, which
/// is not laid out, so those come out a little smaller than on the console.
fn console_layout(description: &Description) -> Option<AllocationInfo> {
    let d = description;
    let texture = console_texture(d)?;
    // The layout's own alignment unless the title asks for more: 4 KiB for the 4 KiB
    // modes, where PC would say 64 KiB.
    let alignment = d.alignment.max(texture.alignment());
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(AllocationInfo {
        bytes: texture.bytes().checked_next_multiple_of(alignment)?,
        alignment,
    })
}

/// How the console lays out the texture `description` describes, in the mode its layout
/// field names or the one the console's library would choose. `None` for a buffer and for
/// a texture `riptide-formats` cannot lay out.
pub(crate) fn console_texture(
    description: &Description,
) -> Option<riptide_formats::texture::Texture> {
    use riptide_formats::texture::{Mode, Shape, Texture, TextureDesc, Usage, suggested_mode};
    let d = description;
    let shape = match d.dimension {
        2 => Shape::OneD,
        3 => Shape::TwoD,
        4 => Shape::ThreeD,
        _ => return None,
    };
    let mode = if d.layout & 0x100 != 0 {
        Mode::from_number(d.layout & 0xff)?
    } else if d.layout == 1 {
        Mode::Linear
    } else {
        let usage = if d.flags & 2 != 0 {
            Usage::DepthStencil
        } else if d.flags & 1 != 0 {
            Usage::RenderTarget
        } else {
            Usage::Sampled
        };
        suggested_mode(shape, usage)
    };
    Texture::new(&TextureDesc {
        shape,
        width: u32::try_from(d.width).ok()?,
        height: d.height,
        depth_or_slices: u32::from(d.depth_or_array_size).max(1),
        mips: u32::from(d.mip_levels),
        format: d.format,
        samples: d.sample_count,
        mode,
    })
    .ok()
}

/// The memory a resource takes: a buffer its width in whole 64 KiB, and a texture what the
/// console's own layout of it takes ([`console_layout`]). A title keeps these sizes in its
/// own tables and places its textures by them.
///
/// A texture the console's layouts are not known for (a format `riptide-formats` does not
/// have) is sized as every level of every slice and sample packed one after another, in
/// whole 64 KiB, which is less than the console would say.
pub(crate) fn footprint(description: &Description) -> Option<AllocationInfo> {
    let d = description;
    if d.dimension != DIMENSION_BUFFER
        && let Some(laid_out) = console_layout(d)
    {
        return Some(laid_out);
    }
    let bytes = if d.dimension == DIMENSION_BUFFER {
        d.width
    } else {
        let (block_bytes, block_texels) = block(d.format)?;
        let (depth, slices) = if d.dimension == DIMENSION_TEXTURE_3D {
            (u64::from(d.depth_or_array_size), 1)
        } else {
            (1, u64::from(d.depth_or_array_size).max(1))
        };
        let mut chain = 0u64;
        for level in 0..level_count(d).min(64) {
            let width = (d.width >> level).max(1).div_ceil(block_texels);
            let height = (u64::from(d.height) >> level).max(1).div_ceil(block_texels);
            let depth = (depth >> level).max(1);
            let level_bytes = width
                .checked_mul(height)?
                .checked_mul(depth)?
                .checked_mul(block_bytes)?;
            chain = chain.checked_add(level_bytes)?;
        }
        chain
            .checked_mul(slices)?
            .checked_mul(u64::from(d.sample_count.max(1)))?
    };
    if bytes == 0 {
        return None;
    }
    let least = if d.sample_count > 1 { MIB_4 } else { KIB_64 };
    let alignment = d.alignment.max(least);
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(AllocationInfo {
        bytes: bytes.checked_next_multiple_of(KIB_64)?,
        alignment,
    })
}

/// Places resources one after another, each at the next multiple of its alignment, tells
/// `each` where every one went, and returns what they take together.
fn place<'a>(
    descriptions: impl Iterator<Item = &'a Description>,
    mut each: impl FnMut(usize, Placement),
) -> AllocationInfo {
    let mut end = 0u64;
    let mut alignment = KIB_64;
    for (index, description) in descriptions.enumerate() {
        let placed = footprint(description).and_then(|one| {
            let offset = end.checked_next_multiple_of(one.alignment)?;
            Some((offset, one, offset.checked_add(one.bytes)?))
        });
        let Some((offset, one, after)) = placed else {
            trace!("a resource that cannot be sized was asked about: {description:?}");
            each(
                index,
                Placement {
                    offset: u64::MAX,
                    alignment: 0,
                    bytes: u64::MAX,
                },
            );
            return UNKNOWN;
        };
        each(
            index,
            Placement {
                offset,
                alignment: one.alignment,
                bytes: one.bytes,
            },
        );
        end = after;
        alignment = alignment.max(one.alignment);
    }
    match end.checked_next_multiple_of(alignment) {
        Some(bytes) => AllocationInfo { bytes, alignment },
        None => UNKNOWN,
    }
}

/// Says what was asked and answered, for the first few distinct questions.
fn show_sizes(descriptions: &[&Description], answer: AllocationInfo) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SHOWN: AtomicU32 = AtomicU32::new(0);
    if SHOWN.fetch_add(1, Ordering::Relaxed) < 12 {
        trace!("resources sized: {descriptions:?} take {answer:x?}");
    }
}

/// `ID3D12Device::GetResourceAllocationInfo`: `(visible nodes, count, descriptions)`, with
/// the result returned through a pointer passed before the device, as
/// [`custom_heap_properties`] does.
///
/// # Safety
/// `out` is null or room for the result; `descriptions` is null or `count` descriptions.
pub(crate) unsafe extern "system" fn allocation_info(
    out: *mut AllocationInfo,
    _this: *const Device,
    _visible: u32,
    count: u32,
    descriptions: *const Description,
) -> *mut AllocationInfo {
    let descriptions: Vec<&Description> = if descriptions.is_null() {
        Vec::new()
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descriptions, count as usize) }
            .iter()
            .collect()
    };
    let total = place(descriptions.iter().copied(), |_, _| {});
    show_sizes(&descriptions, total);
    // SAFETY: the caller's result pointer; null is tolerated.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = total;
    }
    out
}

/// `D3D12_RESOURCE_DESC1`: the description, then a mip region for sampler feedback.
#[repr(C)]
pub(crate) struct Description1 {
    description: Description,
    mip_region: [u32; 3],
}

const _: () = assert!(size_of::<Description1>() == 72);

/// `ID3D12Device8::GetResourceAllocationInfo2`: as [`allocation_info`] for the newer
/// descriptions, also telling where each resource goes.
///
/// # Safety
/// As [`allocation_info`]; `each` is null or room for `count` placements.
pub(crate) unsafe extern "system" fn allocation_info2(
    out: *mut AllocationInfo,
    _this: *const Device,
    _visible: u32,
    count: u32,
    descriptions: *const Description1,
    each: *mut Placement,
) -> *mut AllocationInfo {
    let descriptions: Vec<&Description> = if descriptions.is_null() {
        Vec::new()
    } else {
        // SAFETY: the title passes `count` descriptions.
        unsafe { std::slice::from_raw_parts(descriptions, count as usize) }
            .iter()
            .map(|one| &one.description)
            .collect()
    };
    let total = place(descriptions.iter().copied(), |index, placement| {
        if !each.is_null() {
            // SAFETY: the title passes room for `count` placements.
            unsafe { each.add(index).write_unaligned(placement) };
        }
    });
    show_sizes(&descriptions, total);
    // SAFETY: the caller's result pointer; null is tolerated.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = total;
    }
    out
}

/// A resource as a title's code sees it.
///
/// On the console a resource is a description of some memory: its GPU address is the
/// address the title's CPU code reaches it at. So a resource here records where in title
/// memory it lies, and the host objects that show that memory to the GPU are found when a
/// command or a view first needs them.
#[repr(C)]
pub(crate) struct Resource {
    object: Object,
    description: Description,
    heap: HeapProperties,
    heap_flags: u32,
    /// The title address of its first byte.
    address: u64,
    /// Whose its memory is, and so who gets it back when the resource goes.
    owned: Owned,
    /// A texture's host texture, made the first time a command needs it; `None` inside if
    /// the host could not make one.
    host: OnceLock<Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource>>,
}

/// Where a resource's memory came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owned {
    /// The title's: it placed the resource there itself.
    No,
    /// Reserved or committed here, for a committed resource of a title without an
    /// allocator of its own.
    Here,
    /// From the title's allocator, with these attributes.
    Title(u64),
}

impl Drop for Resource {
    fn drop(&mut self) {
        // Its views in the shaders' heap go with it, and it is no longer kept filled.
        if self.host.get().is_some_and(Option::is_some) {
            crate::bindless::forget(std::ptr::from_ref(self) as usize);
            crate::upload::forget(std::ptr::from_ref(self) as usize);
        }
        let Some(services) = crate::services() else {
            return;
        };
        match self.owned {
            Owned::No => {}
            Owned::Here => {
                if let Err(error) = services.memory.release(self.address) {
                    trace!(
                        "a resource's memory at {:#x} was not given back: {error:?}",
                        self.address
                    );
                }
            }
            Owned::Title(attributes) => {
                if let Some((_, free)) = (services.allocator)() {
                    // SAFETY: the address came from the title's allocation routine with
                    // these attributes, and nothing uses it any more.
                    unsafe { free(self.address as *mut c_void, attributes) };
                }
            }
        }
    }
}

// SAFETY: `Resource` is `#[repr(C)]` and starts with its header.
unsafe impl Child for Resource {
    const NAME: &'static str = "resource";
    const IIDS: &'static [Iid] = &IIDS;

    fn header(&self) -> &Object {
        &self.object
    }
}

impl Resource {
    pub(crate) fn description(&self) -> &Description {
        &self.description
    }

    /// The host texture of a texture, made on first use. `None` for a buffer, which the
    /// host reads in title memory, and for a texture the host could not make.
    pub(crate) fn host(&self) -> Option<&windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
        self.host
            .get_or_init(|| crate::target::make(&self.description))
            .as_ref()
    }

    /// The title address of its first byte, which is also its GPU address.
    pub(crate) fn address(&self) -> u64 {
        self.address
    }
}

/// The resource for memory a title placed itself and describes in a view, with no resource
/// object of its own: one per address and description, made the first time it is named and
/// kept for the run, since records anywhere in title memory may name it.
pub(crate) fn placed(address: u64, description: &Description) -> *const Resource {
    use std::collections::HashMap;
    use std::sync::Mutex;
    /// The description's fields, without the padding between them.
    type Key = (u64, [u64; 2], [u32; 7], [u16; 2]);
    static PLACED: Mutex<Option<HashMap<Key, usize>>> = Mutex::new(None);
    let d = description;
    let key: Key = (
        address,
        [d.alignment, d.width],
        [
            d.dimension,
            d.height,
            d.format,
            d.sample_count,
            d.sample_quality,
            d.layout,
            d.flags,
        ],
        [d.depth_or_array_size, d.mip_levels],
    );
    let mut placed = PLACED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let at = *placed
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| {
            Box::into_raw(Box::new(Resource {
                object: Object::new(table(), 1),
                description: *description,
                heap: HeapProperties {
                    kind: HEAP_DEFAULT,
                    cpu_pages: 0,
                    pool: 0,
                    creation_nodes: 1,
                    visible_nodes: 1,
                },
                heap_flags: 0,
                address,
                owned: Owned::No,
                host: OnceLock::new(),
            })) as usize
        });
    at as *const Resource
}

/// More positions than the resource's table has.
const SLOTS: usize = 24;

/// The resource's methods by position. Positions 0 to 14 are the PC resource's.
const NAMES: [(usize, &str); 15] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
    (8, "Map"),
    (9, "Unmap"),
    (10, "GetDesc"),
    (11, "GetGPUVirtualAddress"),
    (12, "WriteToSubresource"),
    (13, "ReadFromSubresource"),
    (14, "GetHeapProperties"),
];

fn table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut methods = object::child_methods::<Resource>().to_vec();
        methods.extend([
            (8, map as *const () as usize),
            (9, unmap as *const () as usize),
            (10, describe as *const () as usize),
            (11, gpu_address as *const () as usize),
            (14, heap_properties as *const () as usize),
        ]);
        object::table("ID3D12Resource", SLOTS, &NAMES, &methods)
    })
}

/// `ID3D12Device::CreateResourceAtAddressX`: `(address, description, first layout, clear
/// value, interface id, resource out)`. The resource is memory the title has mapped itself.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create_at_address(
    _device: *const Device,
    address: u64,
    description: *const Description,
    layout: u32,
    _clear: *const c_void,
    iid: *const Iid,
    out: *mut *const Resource,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(&description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    if address == 0 {
        return E_INVALIDARG;
    }
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let resource = Box::new(Resource {
        object: Object::new(table(), 1),
        description,
        heap: HeapProperties {
            kind: HEAP_DEFAULT,
            cpu_pages: 0,
            pool: 0,
            creation_nodes: 1,
            visible_nodes: 1,
        },
        heap_flags: 0,
        address,
        owned: Owned::No,
        host: OnceLock::new(),
    });
    trace!(
        "resource {:p} made at address {address:#x}, first layout {layout:#x}: {description:?}",
        &raw const *resource
    );
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(resource, out) }
}

/// Whether the CPU reaches the memory of a heap with these properties: upload and
/// read-back heaps, and custom heaps whose pages are not marked as out of its reach.
fn cpu_reaches(heap: &HeapProperties) -> bool {
    match heap.kind {
        HEAP_UPLOAD | HEAP_READBACK => true,
        HEAP_CUSTOM => heap.cpu_pages > CPU_PAGES_NOT_AVAILABLE,
        _ => false,
    }
}

/// Whether a committed resource gets memory, or addresses only.
///
/// A buffer always gets memory: the GPU uses it where it lies. So does anything in a heap
/// the CPU reaches. A texture in a heap the CPU does not reach is drawn to and sampled by
/// the GPU alone, which keeps it in video memory, so nothing ever lies at its address; it
/// still gets addresses of its own, because its address is how the title and its
/// descriptors name it.
fn needs_memory(description: &Description, heap: &HeapProperties) -> bool {
    description.dimension == DIMENSION_BUFFER || cpu_reaches(heap)
}

/// The attributes a committed resource's memory is asked of the title's allocator with:
/// the graphics allocator's number (128) from bit 16, the page size in bits 14 and 15 (2 MiB
/// pages from 2 MiB up, else 64 KiB), the alignment as a power of two from bit 24, and the
/// kind of memory from bit 29 (write-combined for heaps the CPU does not reach, cached
/// for the others).
///
/// Which page size and kind of memory the console's own runtime asks for is not known.
/// These are what such memory is on the console.
fn attributes(size: &AllocationInfo, heap: &HeapProperties) -> u64 {
    const GRAPHICS_ALLOCATOR: u64 = 128 << 16;
    let pages: u64 = if size.bytes >= 2 << 20 { 2 } else { 1 };
    let memory: u64 = if cpu_reaches(heap) { 3 } else { 1 };
    let alignment = u64::from(size.alignment.max(1).trailing_zeros());
    GRAPHICS_ALLOCATOR | (pages << 14) | (alignment << 24) | (memory << 29)
}

/// Where a committed resource's memory may lie: above 4 GiB and below 2^37 (where the GPU's page table ends), as the title's
/// other graphics memory does.
fn limits(alignment: u64) -> Limits {
    Limits {
        lowest: 4 << 30,
        highest: (1 << 37) - 1,
        alignment: if alignment > KIB_64 { alignment } else { 0 },
    }
}

/// `ID3D12Device::CreateCommittedResource`: `(heap properties, heap flags, description,
/// first state, clear value, interface id, resource out)`. The resource gets title memory
/// of its own.
///
/// # Safety
/// The title's pointers: each null or valid.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) unsafe extern "system" fn create_committed(
    _device: *const Device,
    heap: *const HeapProperties,
    heap_flags: u32,
    description: *const Description,
    state: u32,
    _clear: *const c_void,
    iid: *const Iid,
    out: *mut *const Resource,
) -> Hresult {
    // SAFETY: the title's descriptions; null is refused.
    let (Some(&heap), Some(&description)) = (unsafe { (heap.as_ref(), description.as_ref()) })
    else {
        return E_INVALIDARG;
    };
    let Some(size) = footprint(&description) else {
        trace!("a committed resource that cannot be sized was asked for: {description:?}");
        return E_INVALIDARG;
    };
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let Some(services) = crate::services() else {
        return E_OUTOFMEMORY;
    };
    let with_memory = needs_memory(&description, &heap);
    let (address, owned, from) = if let Some((allocate, _)) = (services.allocator)() {
        // The title's allocator gives the memory, as on the console, and the title's own
        // books of its graphics memory see it.
        let attributes = attributes(&size, &heap);
        // SAFETY: the title's own routine, called as the console's runtime calls it.
        let address = unsafe { allocate(size.bytes as usize, attributes) } as u64;
        if address == 0 {
            trace!(
                "the title's allocator gave no memory for a committed resource ({:#x} \
                 bytes, attributes {attributes:#x}): {description:?}",
                size.bytes
            );
            return E_OUTOFMEMORY;
        }
        if with_memory {
            // A new resource reads as zero. One the GPU alone uses is not touched, so
            // that its pages take no RAM.
            // SAFETY: the allocator handed out that many bytes.
            unsafe { (address as *mut u8).write_bytes(0, size.bytes as usize) };
        }
        (address, Owned::Title(attributes), "the title's allocator")
    } else {
        let limits = limits(size.alignment);
        let reserved = if with_memory {
            services
                .memory
                .commit(None, size.bytes, limits, Access::ReadWrite)
        } else {
            services.memory.reserve(size.bytes, limits)
        };
        match reserved {
            Ok(address) if with_memory => (address, Owned::Here, "memory committed here"),
            Ok(address) => (address, Owned::Here, "addresses reserved here"),
            Err(error) => {
                trace!("no memory for a committed resource ({description:?}): {error:?}");
                return E_OUTOFMEMORY;
            }
        }
    };
    let resource = Box::new(Resource {
        object: Object::new(table(), 1),
        description,
        heap,
        heap_flags,
        address,
        owned,
        host: OnceLock::new(),
    });
    trace!(
        "resource {:p} committed at {address:#x}, {:#x} bytes from {from}, heap {heap:?} \
         flags {heap_flags:#x}, first state {state:#x}: {description:?}",
        &raw const *resource, size.bytes
    );
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(resource, out) }
}

/// `ID3D12Device10::CreateCommittedResource3`: `(heap properties, heap flags, description,
/// first layout, clear value, protected session, castable format count, castable formats,
/// interface id, resource out)`. As [`create_committed`], with a layout where that has a
/// state. The newer description starts like the older one.
///
/// # Safety
/// The title's pointers: each null or valid.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) unsafe extern "system" fn create_committed3(
    device: *const Device,
    heap: *const HeapProperties,
    heap_flags: u32,
    description: *const Description,
    layout: u32,
    clear: *const c_void,
    _protected: *const c_void,
    _castable_count: u32,
    _castable: *const u32,
    iid: *const Iid,
    out: *mut *const Resource,
) -> Hresult {
    // SAFETY: as received.
    unsafe {
        create_committed(
            device,
            heap,
            heap_flags,
            description,
            layout,
            clear,
            iid,
            out,
        )
    }
}

/// `Map`: `(subresource, range read, pointer out)`. The memory is the title's and always
/// mapped; a null `out` only asks that it be.
///
/// # Safety
/// `out` is null or where the pointer goes.
unsafe extern "system" fn map(
    this: &Resource,
    _subresource: u32,
    _read: *const c_void,
    out: *mut u64,
) -> Hresult {
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = this.address;
    }
    S_OK
}

/// `Unmap`: `(subresource, range written)`. Nothing to do: the GPU reads the same memory.
extern "system" fn unmap(_this: &Resource, _subresource: u32, _written: *const c_void) {}

/// `GetDesc`.
///
/// # Safety
/// As [`object::this_and_result`]; the result pointer is null or room for a description.
unsafe extern "system" fn describe(
    first: *const Resource,
    second: *const Resource,
) -> *mut Description {
    // SAFETY: as received.
    let (resource, out) =
        unsafe { object::this_and_result::<Resource, Description>(table(), first, second) };
    // SAFETY: the caller's result pointer; null is tolerated.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = resource.description;
    }
    out
}

/// `GetGPUVirtualAddress`: the title address.
extern "system" fn gpu_address(this: &Resource) -> u64 {
    this.address
}

/// `GetHeapProperties`: `(properties out, heap flags out)`.
///
/// # Safety
/// Each pointer is null or room for its value.
unsafe extern "system" fn heap_properties(
    this: &Resource,
    properties: *mut HeapProperties,
    flags: *mut u32,
) -> Hresult {
    // SAFETY: the title's out-pointers; null is tolerated.
    unsafe {
        if let Some(properties) = properties.as_mut() {
            *properties = this.heap;
        }
        if let Some(flags) = flags.as_mut() {
            *flags = this.heap_flags;
        }
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, method};

    fn texture(width: u64, height: u32, levels: u16, format: u32) -> Description {
        Description {
            dimension: 3,
            alignment: 0,
            width,
            height,
            depth_or_array_size: 1,
            mip_levels: levels,
            format,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags: 0,
        }
    }

    #[test]
    fn only_what_the_cpu_or_the_gpu_reads_in_place_gets_memory() {
        let buffer = Description {
            dimension: DIMENSION_BUFFER,
            width: 4096,
            ..texture(0, 1, 1, 0)
        };
        let target = texture(3840, 2160, 1, 28);
        let default = as_custom(HEAP_DEFAULT, 1);
        let upload = as_custom(HEAP_UPLOAD, 1);
        assert!(needs_memory(&buffer, &default));
        assert!(needs_memory(&target, &upload));
        assert!(!needs_memory(&target, &default));
        let plain_default = HeapProperties {
            kind: HEAP_DEFAULT,
            ..default
        };
        assert!(!needs_memory(&target, &plain_default));
        assert_eq!(limits(KIB_64).alignment, 0);
        assert_eq!(limits(MIB_4).alignment, MIB_4);

        // 32 MiB for the GPU alone, in a layout aligned to 4 KiB: graphics allocator,
        // 2 MiB pages, write-combined.
        let size = footprint(&target).unwrap();
        assert_eq!(
            attributes(&size, &default),
            (128 << 16) | (2 << 14) | (12 << 24) | (1 << 29)
        );
        let small = footprint(&buffer).unwrap();
        assert_eq!(
            attributes(&small, &upload),
            (128 << 16) | (1 << 14) | (16 << 24) | (3 << 29)
        );
    }

    #[test]
    fn a_committed_resource_needs_its_descriptions_and_a_size() {
        let heap = as_custom(HEAP_DEFAULT, 1);
        let unknown = texture(16, 16, 1, 104);
        let mut out: *const Resource = std::ptr::null();
        let (device, clear) = (std::ptr::null(), std::ptr::null());
        // SAFETY: valid pointers, and the null ones under test.
        unsafe {
            assert_eq!(
                create_committed(device, &heap, 0, &unknown, 0, clear, &IIDS[0], &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                create_committed(
                    device,
                    std::ptr::null(),
                    0,
                    &unknown,
                    0,
                    clear,
                    &IIDS[0],
                    &mut out
                ),
                E_INVALIDARG
            );
            let target = texture(64, 64, 1, 28);
            assert_eq!(
                create_committed(device, &heap, 0, &target, 0, clear, &[9; 16], &mut out),
                E_NOINTERFACE
            );
        }
    }

    #[test]
    fn a_buffer_takes_its_width_in_whole_64_kib() {
        let buffer = Description {
            dimension: DIMENSION_BUFFER,
            width: 100_000,
            height: 1,
            ..texture(0, 1, 1, 0)
        };
        assert_eq!(
            footprint(&buffer),
            Some(AllocationInfo {
                bytes: 2 * KIB_64,
                alignment: KIB_64
            })
        );
        assert_eq!(footprint(&Description { width: 0, ..buffer }), None);
    }

    #[test]
    fn a_texture_takes_what_the_consoles_layout_of_it_takes() {
        // The title's back buffer: 3840 by 2160 of 4 bytes, a render target with no mode
        // named. In the 64 KiB render mode a block is 128 by 128 texels, so the texture is
        // 30 by 17 blocks.
        let target = Description {
            flags: 0x30_0005,
            ..texture(3840, 2160, 1, 28)
        };
        assert_eq!(
            footprint(&target),
            Some(AllocationInfo {
                bytes: 30 * 17 * KIB_64,
                alignment: KIB_64
            })
        );
        // A sampled texture with no mode named is laid out in 4 KiB blocks and aligned to
        // 4 KiB: 256 by 128 of one byte is eight blocks.
        assert_eq!(
            footprint(&texture(256, 128, 1, 61)),
            Some(AllocationInfo {
                bytes: 0x8000,
                alignment: 0x1000
            })
        );
        // A mode named in the layout field is the one used, and a larger alignment asked
        // for is given.
        let named = Description {
            layout: 0x100 | 9,
            alignment: 2 << 20,
            ..texture(256, 256, 1, 28)
        };
        let named = footprint(&named).unwrap();
        assert_eq!(named.alignment, 2 << 20);
        assert_eq!(named.bytes % (2 << 20), 0);
        // More levels, slices or samples never take less.
        let one = footprint(&texture(1024, 1024, 1, 71)).unwrap().bytes;
        let chain = footprint(&texture(1024, 1024, 0, 71)).unwrap().bytes;
        let six = Description {
            depth_or_array_size: 6,
            ..texture(1024, 1024, 1, 71)
        };
        assert!(chain > one);
        assert!(footprint(&six).unwrap().bytes >= 6 * one);
    }

    #[test]
    fn a_texture_of_a_format_with_no_known_layout_has_no_size() {
        assert_eq!(footprint(&texture(256, 256, 1, 104)), None);
        assert_eq!(console_layout(&texture(256, 256, 1, 104)), None);
    }

    #[test]
    fn resources_asked_about_together_are_placed_one_after_another() {
        let both = [
            Description1 {
                description: texture(256, 256, 1, 28),
                mip_region: [0; 3],
            },
            Description1 {
                description: Description {
                    alignment: 2 << 20,
                    ..texture(64, 64, 1, 28)
                },
                mip_region: [0; 3],
            },
        ];
        let mut total = UNKNOWN;
        let mut each = [Placement {
            offset: 0,
            alignment: 0,
            bytes: 0,
        }; 2];
        // SAFETY: two descriptions, and room for the results.
        unsafe {
            let returned = allocation_info2(
                &mut total,
                std::ptr::null(),
                0,
                2,
                both.as_ptr(),
                each.as_mut_ptr(),
            );
            assert!(std::ptr::eq(returned, &raw const total));
        }
        let first = footprint(&both[0].description).unwrap();
        let second = footprint(&both[1].description).unwrap();
        assert_eq!((each[0].offset, each[0].bytes), (0, first.bytes));
        // The second starts at the next multiple of its own alignment.
        assert_eq!(second.alignment, 2 << 20);
        assert_eq!(each[1].offset, first.bytes.next_multiple_of(2 << 20));
        assert_eq!(each[1].bytes, second.bytes);
        assert_eq!(total.alignment, 2 << 20);
        assert_eq!(
            total.bytes,
            (each[1].offset + second.bytes).next_multiple_of(2 << 20)
        );

        // One that cannot be sized makes the whole answer unknown.
        let unknown = [texture(256, 256, 1, 104)];
        // SAFETY: one description, and room for the result.
        unsafe { allocation_info(&mut total, std::ptr::null(), 0, 1, unknown.as_ptr()) };
        assert_eq!(total, UNKNOWN);
    }

    #[test]
    fn a_resource_at_an_address_is_that_memory() {
        let description = Description {
            dimension: DIMENSION_BUFFER,
            alignment: 0,
            width: 0x1_0000,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 0,
            sample_count: 1,
            sample_quality: 0,
            layout: 1,
            flags: 0,
        };
        let device = std::ptr::null();
        let clear = std::ptr::null();
        let mut made: *const Resource = std::ptr::null();
        // SAFETY: valid pointers, and the null ones under test; the table has these
        // methods at these positions.
        unsafe {
            assert_eq!(
                create_at_address(device, 0, &description, 0, clear, &IIDS[0], &mut made),
                E_INVALIDARG
            );
            assert_eq!(
                create_at_address(device, 0x4000, &description, 0, clear, &[9; 16], &mut made),
                E_NOINTERFACE
            );
            assert_eq!(
                create_at_address(device, 0x4000, &description, 0, clear, &IIDS[2], &mut made),
                S_OK
            );
            let resource = &*made;
            assert_eq!(gpu_address(resource), 0x4000);
            let mut mapped = 0;
            assert_eq!(map(resource, 0, std::ptr::null(), &mut mapped), S_OK);
            assert_eq!(mapped, 0x4000);
            assert_eq!(
                map(resource, 0, std::ptr::null(), std::ptr::null_mut()),
                S_OK
            );

            // The description comes back whichever side its pointer is on.
            let mut described = [Description {
                width: 0,
                ..description
            }; 2];
            let room = described.as_mut_ptr().cast::<Resource>().cast_const();
            describe(made, room);
            assert_eq!(described[0], description);
            described[0].width = 0;
            describe(room, made);
            assert_eq!(described[0], description);

            let mut heap = as_custom(HEAP_UPLOAD, 1);
            let mut flags = 7;
            assert_eq!(heap_properties(resource, &mut heap, &mut flags), S_OK);
            assert_eq!((heap.kind, flags), (HEAP_DEFAULT, 0));

            let release: unsafe extern "system" fn(*const Resource) -> u32 = method(made.cast(), 2);
            assert_eq!(release(made), 0);
        }
    }

    #[test]
    fn standard_heaps_are_system_memory_with_their_own_cpu_rules() {
        assert_eq!(
            as_custom(HEAP_DEFAULT, 0),
            HeapProperties {
                kind: HEAP_CUSTOM,
                cpu_pages: CPU_PAGES_NOT_AVAILABLE,
                pool: POOL_L0,
                creation_nodes: 1,
                visible_nodes: 1,
            }
        );
        assert_eq!(as_custom(HEAP_UPLOAD, 1).cpu_pages, CPU_PAGES_WRITE_BACK);
        assert_eq!(as_custom(HEAP_READBACK, 1).cpu_pages, CPU_PAGES_WRITE_BACK);
        let unknown = as_custom(9, 1);
        assert_eq!((unknown.cpu_pages, unknown.pool), (0, 0));
    }

    #[test]
    fn the_result_goes_through_the_first_argument() {
        let mut out = as_custom(0, 1);
        // SAFETY: room for the result, and null.
        unsafe {
            let returned = custom_heap_properties(&mut out, std::ptr::null(), 1, HEAP_UPLOAD);
            assert!(std::ptr::eq(returned, &raw const out));
            assert!(custom_heap_properties(std::ptr::null_mut(), std::ptr::null(), 1, 1).is_null());
        }
        assert_eq!(out, as_custom(HEAP_UPLOAD, 1));
    }
}
