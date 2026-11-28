//! Query heaps: where the GPU keeps what a title asks it to count or time.
//!
//! Each console query heap is one host query heap of the same type and size.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_QUERY_HEAP_DESC, D3D12_QUERY_HEAP_TYPE, ID3D12QueryHeap,
};

use crate::device::Device;
use crate::object::{self, Child, E_INVALIDARG, Hresult, Iid, Object, guid};
use crate::trace;

/// `ID3D12QueryHeap`.
const IID: Iid = guid(
    0x0d9658ae,
    0xed45,
    0x469e,
    [0xa6, 0x1d, 0x97, 0x0e, 0xc5, 0x83, 0xca, 0xb4],
);

/// `D3D12_QUERY_HEAP_DESC`, as on PC.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Description {
    /// 0 occlusion, 1 timestamp, 2 pipeline statistics, 3 stream output statistics.
    kind: u32,
    count: u32,
    nodes: u32,
}

/// A query heap as a title's code sees it.
#[repr(C)]
pub(crate) struct QueryHeap {
    object: Object,
    description: Description,
    host: ID3D12QueryHeap,
}

// SAFETY: `QueryHeap` is `#[repr(C)]` and starts with its header.
unsafe impl Child for QueryHeap {
    const NAME: &'static str = "query heap";
    const IIDS: &'static [Iid] = &[IID];

    fn header(&self) -> &Object {
        &self.object
    }
}

impl QueryHeap {
    /// The host heap behind this one, for a list's queries.
    #[allow(dead_code)] // the first users are a list's query methods
    pub(crate) fn host(&self) -> &ID3D12QueryHeap {
        &self.host
    }
}

/// The heap has no methods of its own after the eight every device child has.
const SLOTS: usize = 12;

const NAMES: [(usize, &str); 8] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
];

fn table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let methods = object::child_methods::<QueryHeap>();
        object::table("ID3D12QueryHeap", SLOTS, &NAMES, &methods)
    })
}

/// `ID3D12Device::CreateQueryHeap`: `(description, interface id, heap out)`.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create(
    _device: *const Device,
    description: *const Description,
    iid: *const Iid,
    out: *mut *const QueryHeap,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(&description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let host = match crate::host::host() {
        Ok(host) => host,
        Err(code) => return code,
    };
    let host_description = D3D12_QUERY_HEAP_DESC {
        Type: D3D12_QUERY_HEAP_TYPE(description.kind as i32),
        Count: description.count,
        NodeMask: 0,
    };
    let mut made: Option<ID3D12QueryHeap> = None;
    // SAFETY: a plain call on the host device with a valid description.
    let result = unsafe { host.device.CreateQueryHeap(&host_description, &mut made) };
    let host = match (result, made) {
        (Ok(()), Some(heap)) => heap,
        (result, _) => {
            let code = result.err().map_or(E_INVALIDARG, |error| error.code().0);
            trace!("the host refused a query heap ({description:?}): {code:#x}");
            return code;
        }
    };
    let heap = Box::new(QueryHeap {
        object: Object::new(table(), 1),
        description,
        host,
    });
    trace!("query heap {:p} made: {description:?}", &raw const *heap);
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(heap, out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER};

    #[test]
    fn a_query_heap_is_not_made_as_another_interface_or_without_a_description() {
        let description = Description {
            kind: 1,
            count: 64,
            nodes: 0,
        };
        let mut out: *const QueryHeap = std::ptr::dangling();
        let device = std::ptr::null();
        // SAFETY: valid pointers, and the null ones under test.
        unsafe {
            assert_eq!(
                create(device, &description, &[9u8; 16], &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                create(device, &description, &IID, std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!(
                create(device, std::ptr::null(), &IID, &mut out),
                E_INVALIDARG
            );
        }
    }
}
