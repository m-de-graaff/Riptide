//! Heaps and resources: the memory graphics objects lie in.
//!
//! On the console the CPU and the GPU share one memory, so every heap type is ordinary
//! title memory with different rules about who may touch it.

use crate::device::Device;
use crate::trace;

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

#[cfg(test)]
mod tests {
    use super::*;

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
