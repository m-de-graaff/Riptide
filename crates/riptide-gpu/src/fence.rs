//! Fences: counters a title uses to learn how far the GPU has got.
//!
//! On the console a fence's value is 8 bytes of memory that the GPU can write and wait on,
//! and a title reads it there as well as through the fence's methods. So a fence object
//! lies in title memory, its value at the place the title's code expects, and that value
//! is the truth.
//!
//! The value moves in three ways. The title sets it. A list's immediate write lands in it,
//! since title memory is a host buffer. And a queue's signal: that goes to a host fence,
//! and a watcher thread carries the host fence's progress into the value. The same thread
//! sets the events a title waits with, always after the value, so that a title woken by
//! its event finds the value it waited for.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use riptide_mem::{Access, Limits};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{D3D12_FENCE_FLAGS, ID3D12Fence};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject};

use crate::device::Device;
use crate::object::{self, Child, E_OUTOFMEMORY, Hresult, Iid, Object, S_OK, guid};
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
    /// The fence's value. The title and the GPU read and write it here.
    value: AtomicU64,
    flags: u32,
    /// Whether the object lies in the pool of title memory, or (with no title memory, in
    /// this crate's own tests) on the heap.
    pooled: bool,
    /// The host fence a queue's signal goes to.
    host: ID3D12Fence,
}

const _: () = assert!(std::mem::offset_of!(Fence, value) == 0x10);
const _: () = assert!(size_of::<Fence>() <= SLOT);

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

    /// Tells the watcher that the host queue will bring the host fence to `value`, so that
    /// it carries that into the fence's own value when it happens.
    pub(crate) fn expect(&self, value: u64) {
        let watcher = watcher();
        // SAFETY: a plain call on the host fence with the watcher's event.
        if let Err(error) = unsafe { self.host.SetEventOnCompletion(value, watcher.wake) } {
            trace!("the host fence takes no event for value {value}: {error}");
        }
        watcher.state().signals.push(Signal {
            fence: std::ptr::from_ref(self) as usize,
            host: self.host.clone(),
            value,
        });
        watcher.nudge();
    }

    /// Raises the value to `value` if it is below it.
    fn reach(&self, value: u64) {
        self.value.fetch_max(value, Ordering::AcqRel);
    }
}

/// The bytes a fence takes in the pool.
const SLOT: usize = 128;
/// The pool grows by this much title memory at a time.
const POOL_STEP: u64 = 64 << 10;

/// Title memory for fence objects: places that were given back, and the unused rest of
/// the last piece committed.
struct Pool {
    free: Vec<usize>,
    next: usize,
    end: usize,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    free: Vec::new(),
    next: 0,
    end: 0,
});

/// A place for a fence in title memory, or `None` if there is no title memory to be had.
fn pool_slot() -> Option<*mut Fence> {
    let services = crate::services()?;
    let mut pool = POOL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(slot) = pool.free.pop() {
        return Some(slot as *mut Fence);
    }
    if pool.next + SLOT > pool.end {
        let limits = Limits {
            lowest: 4 << 30,
            highest: (1 << 40) - 1,
            alignment: 0,
        };
        let start = services
            .memory
            .commit(None, POOL_STEP, limits, Access::ReadWrite)
            .ok()? as usize;
        pool.next = start;
        pool.end = start + POOL_STEP as usize;
    }
    let slot = pool.next;
    pool.next += SLOT;
    Some(slot as *mut Fence)
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
        // Every child's methods but its release: a fence is not always a heap box.
        let mut methods: Vec<(usize, usize)> = object::child_methods::<Fence>()
            .into_iter()
            .filter(|&(slot, _)| slot != 2)
            .collect();
        methods.extend([
            (2, release as *const () as usize),
            (8, completed_value as *const () as usize),
            (9, set_event_on_completion as *const () as usize),
            (10, signal as *const () as usize),
            (11, creation_flags as *const () as usize),
            (12, gpu_address as *const () as usize),
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
    let Some(slot) = pool_slot() else {
        trace!("no title memory for a fence");
        return E_OUTOFMEMORY;
    };
    // SAFETY: the slot is unused title memory of `SLOT` bytes, aligned to 128.
    unsafe {
        slot.write(Fence {
            object: Object::new(table(), 1),
            value: AtomicU64::new(initial),
            flags,
            pooled: true,
            host,
        });
    }
    trace!("fence {slot:p} made: initial {initial}, flags {flags:#x}");
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { out.write(slot) };
    S_OK
}

/// `Release`: the fence goes with its last reference, and its place is free again.
///
/// # Safety
/// `this` is a live fence and the caller gives up one reference.
unsafe extern "system" fn release(this: *mut Fence) -> u32 {
    // SAFETY: a live fence.
    let left = unsafe { &*this }.object.release();
    if left == 0 {
        // Nothing may name the fence once it is gone.
        if let Some(watcher) = WATCHER.get() {
            let mut state = watcher.state();
            state.signals.retain(|signal| signal.fence != this as usize);
            state.waits.retain(|wait| wait.fence != this as usize);
        }
        // SAFETY: the last reference; the fence was written whole where it lies.
        unsafe {
            if (*this).pooled {
                std::ptr::drop_in_place(this);
                POOL.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .free
                    .push(this as usize);
            } else {
                drop(Box::from_raw(this));
            }
        }
    }
    left
}

/// `GetCompletedValue`: how far the fence has got. The host fence is asked first, so that
/// the answer is never behind what the GPU has done.
extern "system" fn completed_value(this: &Fence) -> u64 {
    // SAFETY: a plain call on the host fence.
    let host = unsafe { this.host.GetCompletedValue() };
    // A removed device answers with all ones, which is not a value to keep.
    if host != u64::MAX {
        this.reach(host);
    }
    this.value.load(Ordering::Acquire)
}

/// `SetEventOnCompletion`: sets the title's event when the fence reaches `value`. Without
/// an event the call returns when it has.
extern "system" fn set_event_on_completion(
    this: &Fence,
    value: u64,
    event: *mut c_void,
) -> Hresult {
    if completed_value(this) >= value {
        if !event.is_null() {
            // SAFETY: the title's event handle.
            let _ = unsafe { SetEvent(HANDLE(event)) };
        }
        return S_OK;
    }
    let watcher = watcher();
    // With no event of the title's, one of ours, waited for here.
    let own = if event.is_null() {
        // SAFETY: a new unnamed event.
        match unsafe { CreateEventW(None, false, false, None) } {
            Ok(own) => Some(own),
            Err(error) => return error.code().0,
        }
    } else {
        None
    };
    watcher.state().waits.push(Wait {
        fence: std::ptr::from_ref(this) as usize,
        value,
        event: own.map_or(event as usize, |own| own.0 as usize),
    });
    watcher.nudge();
    if let Some(own) = own {
        // SAFETY: our own event, closed once it has been set.
        unsafe {
            WaitForSingleObject(own, INFINITE);
            let _ = windows::Win32::Foundation::CloseHandle(own);
        }
    }
    S_OK
}

/// `Signal`: the title sets the fence's value itself.
extern "system" fn signal(this: &Fence, value: u64) -> Hresult {
    this.value.store(value, Ordering::Release);
    // The host fence follows, so that a queue's wait on it sees the same. It refuses to go
    // backwards, which changes nothing here.
    // SAFETY: a plain call on the host fence.
    let _ = unsafe { this.host.Signal(value) };
    if let Some(watcher) = WATCHER.get() {
        watcher.nudge();
    }
    S_OK
}

/// `GetCreationFlags`.
extern "system" fn creation_flags(this: &Fence) -> u32 {
    this.flags
}

/// `GetGpuVirtualAddressX`: where the value lies. The GPU's addresses are the title's, and
/// the value is in title memory, so a list can write it and wait on it.
extern "system" fn gpu_address(this: &Fence) -> u64 {
    (&raw const this.value) as u64
}

/// A queue's signal that has not happened yet.
struct Signal {
    fence: usize,
    host: ID3D12Fence,
    value: u64,
}

/// An event to set when a fence's value reaches `value`.
struct Wait {
    fence: usize,
    value: u64,
    event: usize,
}

#[derive(Default)]
struct Watched {
    signals: Vec<Signal>,
    waits: Vec<Wait>,
}

/// The thread that carries host fences' progress into fences' values and sets the events
/// waiting on them.
struct Watcher {
    state: Mutex<Watched>,
    /// Set when there is something new to look at.
    wake: HANDLE,
}

// SAFETY: the handle is process-wide, the host fences are free-threaded, and the fence
// addresses are only followed while the state is locked, which a fence's release also
// takes before the fence goes.
unsafe impl Send for Watcher {}
// SAFETY: as above.
unsafe impl Sync for Watcher {}

static WATCHER: OnceLock<Watcher> = OnceLock::new();

impl Watcher {
    fn state(&self) -> std::sync::MutexGuard<'_, Watched> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn nudge(&self) {
        // SAFETY: our own event.
        let _ = unsafe { SetEvent(self.wake) };
    }

    /// One look at everything watched. Returns whether events are still waited for whose
    /// value only a write to memory can bring, which nothing announces.
    fn look(&self) -> bool {
        let mut state = self.state();
        state.signals.retain(|signal| {
            // SAFETY: a plain call on the host fence.
            let reached = unsafe { signal.host.GetCompletedValue() };
            if reached == u64::MAX || reached < signal.value {
                return reached != u64::MAX;
            }
            // SAFETY: the fence is live: its release removes it from this list first,
            // under the lock held here.
            unsafe { &*(signal.fence as *const Fence) }.reach(signal.value);
            false
        });
        state.waits.retain(|wait| {
            // SAFETY: as above.
            let fence = unsafe { &*(wait.fence as *const Fence) };
            if fence.value.load(Ordering::Acquire) < wait.value {
                return true;
            }
            // SAFETY: the title's event handle, or ours.
            let _ = unsafe { SetEvent(HANDLE(wait.event as *mut c_void)) };
            false
        });
        !state.waits.is_empty()
    }
}

/// The watcher, started the first time it is needed.
fn watcher() -> &'static Watcher {
    WATCHER.get_or_init(|| {
        // SAFETY: a new unnamed event. Without it nothing here can work at all.
        let wake = unsafe { CreateEventW(None, false, false, None) }
            .expect("Windows gives an event for the fence watcher");
        std::thread::Builder::new()
            .name("riptide fences".to_owned())
            .spawn(|| {
                let watcher = watcher();
                let mut waiting = false;
                loop {
                    // A value a list writes straight to memory is announced by nothing,
                    // so while events wait the values are looked at every millisecond.
                    let timeout = if waiting { 1 } else { INFINITE };
                    // SAFETY: our own event.
                    unsafe { WaitForSingleObject(watcher.wake, timeout) };
                    waiting = watcher.look();
                }
            })
            .expect("Windows starts the fence watcher thread");
        Watcher {
            state: Mutex::new(Watched::default()),
            wake,
        }
    })
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

    #[test]
    fn a_fence_fits_its_place_in_the_pool_with_its_value_where_titles_read_it() {
        assert!(size_of::<Fence>() <= SLOT);
        assert_eq!(std::mem::offset_of!(Fence, value), 0x10);
        assert_eq!(POOL_STEP as usize % SLOT, 0);
    }
}
