//! Descriptor heaps: arrays of the small records that tell shaders and commands what a
//! resource is and how to look at it.
//!
//! On the console a descriptor handle is the address of the record, the same for the CPU
//! and the GPU, and a heap is plain memory. So a heap here is a range of title memory: the
//! title's own if it passes one, else a range committed for it. Being title memory, the GPU
//! can read it where it lies, which is what decoding descriptors in shaders (decision D5)
//! needs.

use std::sync::OnceLock;

use riptide_mem::{Access, Limits};

use crate::device::Device;
use crate::object::{self, Child, E_INVALIDARG, E_OUTOFMEMORY, Hresult, Iid, Object, guid};
use crate::trace;

/// `ID3D12DescriptorHeap`.
const IID: Iid = guid(
    0x8efb471d,
    0x616c,
    0x4f49,
    [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
);

/// Heap types: views of resources for shaders, samplers, render targets, depth targets.
pub(crate) const VIEWS: u32 = 0;
pub(crate) const SAMPLERS: u32 = 1;

/// The bytes one descriptor of each heap type takes: 32 for a view and 16 for a sampler,
/// the sizes of the console GPU's own image and sampler records, so a title that copies
/// descriptors as bytes moves whole ones. Render and depth target records are read only
/// here, never by the GPU, and take 48: the resource and the title's description.
pub(crate) const fn increment(kind: u32) -> Option<u32> {
    match kind {
        VIEWS => Some(32),
        SAMPLERS => Some(16),
        2 | 3 => Some(48),
        _ => None,
    }
}

/// `ID3D12Device::GetDescriptorHandleIncrementSize`.
pub(crate) extern "system" fn increment_size(_this: &Device, kind: u32) -> u32 {
    increment(kind).unwrap_or(0)
}

/// `D3D12_DESCRIPTOR_HEAP_DESC`, as on PC.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Description {
    kind: u32,
    count: u32,
    /// Bit 0: shaders read the heap.
    flags: u32,
    nodes: u32,
}

/// What a newer title's description goes on with: memory of its own for the descriptors.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Placement {
    address: u64,
    bytes: u64,
}

/// A descriptor heap as a title's code sees it.
#[repr(C)]
pub(crate) struct Heap {
    object: Object,
    description: Description,
    /// The address of the first descriptor.
    start: u64,
    /// Whether the memory was committed here, and goes with the heap.
    owned: bool,
}

// SAFETY: `Heap` is `#[repr(C)]` and starts with its header.
unsafe impl Child for Heap {
    const NAME: &'static str = "descriptor heap";
    const IIDS: &'static [Iid] = &[IID];

    fn header(&self) -> &Object {
        &self.object
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        if let Some(services) = crate::services()
            && let Err(error) = services.memory.release(self.start)
        {
            trace!(
                "a descriptor heap's memory at {:#x} was not given back: {error:?}",
                self.start
            );
        }
    }
}

impl Heap {
    /// Which descriptor of this heap `handle` is, if it is one.
    #[allow(dead_code)] // the first user is view creation
    pub(crate) fn index(&self, handle: u64) -> Option<u32> {
        index(
            self.start,
            self.description.kind,
            self.description.count,
            handle,
        )
    }
}

fn index(start: u64, kind: u32, count: u32, handle: u64) -> Option<u32> {
    let step = u64::from(increment(kind)?);
    let offset = handle.checked_sub(start)?;
    if offset % step != 0 {
        return None;
    }
    u32::try_from(offset / step)
        .ok()
        .filter(|&index| index < count)
}

/// More positions than the heap's table has.
const SLOTS: usize = 16;

const NAMES: [(usize, &str); 11] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
    (8, "GetDesc"),
    (9, "GetCPUDescriptorHandleForHeapStart"),
    (10, "GetGPUDescriptorHandleForHeapStart"),
];

fn table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut methods = object::child_methods::<Heap>().to_vec();
        methods.extend([
            (8, describe as *const () as usize),
            (9, start as *const () as usize),
            (10, start as *const () as usize),
        ]);
        object::table("ID3D12DescriptorHeap", SLOTS, &NAMES, &methods)
    })
}

/// Where a heap's own memory may lie: below 4 GiB. A descriptor table reaches a shader
/// program as a 32-bit address, whose high half the program's own code makes zero, so a
/// heap anywhere else could not be read. The title places its own view heap there too.
const LIMITS: Limits = Limits {
    lowest: 0x1_0000,
    highest: 0xffff_ffff,
    alignment: 0,
};

/// The memory the title gave for a heap of `bytes`, if the 16 bytes after its description
/// are that: an address and exactly the size the heap needs, lying in title memory.
///
/// A title that passes the plain description has something else after it, so this reads
/// those bytes as a guess and believes them only if all of that holds.
///
/// # Safety
/// `description` points at a heap description with 16 readable bytes after it.
unsafe fn placement(description: *const Description, bytes: u64) -> Option<u64> {
    // SAFETY: as this function's contract says.
    let placement = unsafe { description.add(1).cast::<Placement>().read_unaligned() };
    if placement.address == 0 || placement.bytes != bytes {
        return None;
    }
    let memory = crate::services()?.memory;
    memory
        .locate(placement.address, bytes)
        .map(|_| placement.address)
}

/// `ID3D12Device::CreateDescriptorHeap`: `(description, interface id, heap out)`.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create(
    _device: *const Device,
    description: *const Description,
    iid: *const Iid,
    out: *mut *const Heap,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(&wanted) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    let Some(step) = increment(wanted.kind) else {
        trace!("a descriptor heap of unknown type was asked for: {wanted:?}");
        return E_INVALIDARG;
    };
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let bytes = u64::from(step) * u64::from(wanted.count.max(1));
    // SAFETY: the description was read above; the title's own memory follows it.
    let (start, owned) = match unsafe { placement(description, bytes) } {
        Some(address) => (address, false),
        None => {
            let Some(services) = crate::services() else {
                return E_OUTOFMEMORY;
            };
            match services
                .memory
                .commit(None, bytes, LIMITS, Access::ReadWrite)
            {
                Ok(address) => (address, true),
                Err(error) => {
                    trace!("no memory for a descriptor heap ({wanted:?}): {error:?}");
                    return E_OUTOFMEMORY;
                }
            }
        }
    };
    let heap = Box::new(Heap {
        object: Object::new(table(), 1),
        description: wanted,
        start,
        owned,
    });
    trace!(
        "descriptor heap {:p} made: {wanted:?}, {bytes:#x} bytes at {start:#x} ({})",
        &raw const *heap,
        if owned {
            "committed here"
        } else {
            "the title's own"
        }
    );
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(heap, out) }
}

/// `GetDesc`.
///
/// # Safety
/// As [`object::this_and_result`]; the result pointer is null or room for a description.
unsafe extern "system" fn describe(first: *const Heap, second: *const Heap) -> *mut Description {
    // SAFETY: as received.
    let (heap, out) =
        unsafe { object::this_and_result::<Heap, Description>(table(), first, second) };
    // SAFETY: the caller's result pointer; null is tolerated.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = heap.description;
    }
    out
}

/// `GetCPUDescriptorHandleForHeapStart` and `GetGPUDescriptorHandleForHeapStart`: the
/// address of the first descriptor, which is both handles. It is returned as a value, and
/// also through the result pointer if the title passed one.
///
/// # Safety
/// As [`object::this_and_result`], except that there may be no second argument at all: then the
/// first is the heap and the second is never read.
unsafe extern "system" fn start(first: *const Heap, second: *const Heap) -> u64 {
    let ours = table().as_ptr() as usize;
    // SAFETY: the first argument is the heap or a result pointer, both readable.
    if !first.is_null() && unsafe { first.cast::<usize>().read_unaligned() } == ours {
        // SAFETY: it starts with the heap's table, so it is a heap.
        return unsafe { &*first }.start;
    }
    // SAFETY: the first was not the heap, so it is the result pointer and the second is
    // the heap.
    let (heap, out) = unsafe { object::this_and_result::<Heap, u64>(table(), first, second) };
    // SAFETY: the caller's result pointer.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = heap.start;
    }
    out as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER};

    fn heap(start: u64) -> Heap {
        Heap {
            object: Object::new(table(), 1),
            description: Description {
                kind: VIEWS,
                count: 8,
                flags: 1,
                nodes: 0,
            },
            start,
            owned: false,
        }
    }

    #[test]
    fn views_take_32_bytes_and_samplers_16() {
        let device = crate::device::device();
        assert_eq!(increment_size(device, VIEWS), 32);
        assert_eq!(increment_size(device, SAMPLERS), 16);
        assert_eq!(increment_size(device, 2), 48);
        assert_eq!(increment_size(device, 3), 48);
        assert_eq!(increment_size(device, 4), 0);
    }

    #[test]
    fn a_handle_is_a_descriptor_only_inside_the_heap_and_on_a_boundary() {
        let heap = heap(0x1000);
        assert_eq!(heap.index(0x1000), Some(0));
        assert_eq!(heap.index(0x1000 + 7 * 32), Some(7));
        assert_eq!(heap.index(0x1000 + 8 * 32), None);
        assert_eq!(heap.index(0x1010), None);
        assert_eq!(heap.index(0xfe0), None);
    }

    #[test]
    fn a_heap_is_not_made_of_an_unknown_type_or_as_another_interface() {
        // A plain description with nothing believable after it.
        let description = [
            Description {
                kind: VIEWS,
                count: 4,
                flags: 0,
                nodes: 0,
            },
            Description {
                kind: 0,
                count: 0,
                flags: 0,
                nodes: 0,
            },
        ];
        let unknown = [Description {
            kind: 9,
            ..description[0]
        }; 2];
        let mut out: *const Heap = std::ptr::dangling();
        let device = std::ptr::null();
        // SAFETY: valid pointers, and the null ones under test.
        unsafe {
            assert_eq!(
                create(device, unknown.as_ptr(), &IID, &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                create(device, std::ptr::null(), &IID, &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                create(device, description.as_ptr(), &[9u8; 16], &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                create(device, description.as_ptr(), &IID, std::ptr::null_mut()),
                E_POINTER
            );
        }
    }

    #[test]
    fn the_start_is_found_whichever_side_the_result_pointer_is_on() {
        let heap = heap(0x1234_5000);
        let this = &raw const heap;
        let mut result = 0u64;
        let room = (&raw mut result).cast::<Heap>().cast_const();
        // SAFETY: a live heap and room for the result.
        unsafe {
            assert_eq!(start(this, std::ptr::null()), 0x1234_5000);
            assert_eq!(start(this, room), 0x1234_5000);
            assert_eq!(start(room, this), room as u64);
        }
        assert_eq!(result, 0x1234_5000);

        let mut described = [Description {
            kind: 9,
            count: 0,
            flags: 0,
            nodes: 0,
        }; 2];
        let room = described.as_mut_ptr().cast::<Heap>().cast_const();
        // SAFETY: a live heap and room for the result, on either side.
        unsafe {
            describe(room, this);
            assert_eq!(described[0], heap.description);
            described[0].kind = 9;
            describe(this, room);
            assert_eq!(described[0], heap.description);
        }
    }
}
