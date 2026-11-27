//! Fences: counters a title uses to learn how far the GPU has got.
//!
//! Each console fence is one host fence. A queue's signal goes to the host queue, so the
//! fence moves when the host GPU really gets there, and nothing here waits for it.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{D3D12_FENCE_FLAGS, ID3D12Fence};

use crate::device::Device;
use crate::object::{self, Child, Hresult, Iid, Object, S_OK, guid};
use crate::trace;

/// `ID3D12Fence` and `ID3D12Fence1`.
const IIDS: [Iid; 2] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    guid(
        0x433685fe,
        0xe22b,
        0x4ca0,
        [0xa8, 0xdb, 0xb5, 0xb4, 0xf4, 0xdd, 0x0e, 0x4a],
    ),
];

/// A fence as a title's code sees it: the object header, then the value.
#[repr(C)]
pub(crate) struct Fence {
    object: Object,
    /// The value the fence had the last time it was asked for.
    value: AtomicU64,
    flags: u32,
    host: ID3D12Fence,
}

const _: () = assert!(std::mem::offset_of!(Fence, value) == 0x10);

// SAFETY: `Fence` is `#[repr(C)]` and starts with its header.
unsafe impl Child for Fence {
    const NAME: &'static str = "fence";
    const IIDS: &'static [Iid] = &IIDS;

    fn header(&self) -> &Object {
        &self.object
    }
}

impl Fence {
    /// The host fence behind this one, for a queue to signal.
    pub(crate) fn host(&self) -> &ID3D12Fence {
        &self.host
    }
}

/// More positions than the fence's table has.
const SLOTS: usize = 24;

/// The fence's methods by position. Positions 0 to 11 are the PC fence's.
const NAMES: [(usize, &str); 13] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
    (8, "GetCompletedValue"),
    (9, "SetEventOnCompletion"),
    (10, "Signal"),
    (11, "GetCreationFlags"),
    (12, "GetGpuVirtualAddressX"),
];

fn table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut methods = object::child_methods::<Fence>().to_vec();
        methods.extend([
            (8, completed_value as *const () as usize),
            (9, set_event_on_completion as *const () as usize),
            (10, signal as *const () as usize),
            (11, creation_flags as *const () as usize),
        ]);
        object::table("ID3D12Fence", SLOTS, &NAMES, &methods)
    })
}

/// `ID3D12Device::CreateFence`: `(initial value, flags, interface id, fence out)`.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create(
    _device: *const Device,
    initial: u64,
    flags: u32,
    iid: *const Iid,
    out: *mut *const Fence,
) -> Hresult {
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let host = match crate::host::host() {
        Ok(host) => host,
        Err(code) => return code,
    };
    // SAFETY: a plain call on the host device.
    let made: windows::core::Result<ID3D12Fence> = unsafe {
        host.device
            .CreateFence(initial, D3D12_FENCE_FLAGS(flags as i32))
    };
    let host = match made {
        Ok(fence) => fence,
        Err(error) => {
            trace!("the host refused a fence (initial {initial}, flags {flags:#x}): {error}");
            return error.code().0;
        }
    };
    let fence = Box::new(Fence {
        object: Object::new(table(), 1),
        value: AtomicU64::new(initial),
        flags,
        host,
    });
    trace!(
        "fence {:p} made: initial {initial}, flags {flags:#x}",
        &raw const *fence
    );
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(fence, out) }
}

/// `GetCompletedValue`: how far the GPU has got.
extern "system" fn completed_value(this: &Fence) -> u64 {
    // SAFETY: a plain call on the host fence.
    let value = unsafe { this.host.GetCompletedValue() };
    this.value.store(value, Ordering::Relaxed);
    value
}

/// `SetEventOnCompletion`: sets the title's event when the fence reaches `value`. The event
/// is a Windows event, so the host fence sets it itself.
extern "system" fn set_event_on_completion(
    this: &Fence,
    value: u64,
    event: *mut c_void,
) -> Hresult {
    // SAFETY: a plain call on the host fence with the title's handle.
    match unsafe { this.host.SetEventOnCompletion(value, HANDLE(event)) } {
        Ok(()) => S_OK,
        Err(error) => error.code().0,
    }
}

/// `Signal`: the CPU sets the fence's value itself.
extern "system" fn signal(this: &Fence, value: u64) -> Hresult {
    // SAFETY: a plain call on the host fence.
    match unsafe { this.host.Signal(value) } {
        Ok(()) => {
            this.value.store(value, Ordering::Relaxed);
            S_OK
        }
        Err(error) => error.code().0,
    }
}

/// `GetCreationFlags`.
extern "system" fn creation_flags(this: &Fence) -> u32 {
    this.flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER};

    #[test]
    fn a_fence_is_not_made_as_another_interface() {
        let mut out: *const Fence = std::ptr::dangling();
        // SAFETY: valid pointers, and the null one under test.
        unsafe {
            assert_eq!(
                create(std::ptr::null(), 0, 0, &[9u8; 16], &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            assert_eq!(
                create(std::ptr::null(), 0, 0, &IIDS[0], std::ptr::null_mut()),
                E_POINTER
            );
        }
    }
}
