//! Page pools: memory a title registers so that it can later give its pages, by number, to
//! resources that are made without memory of their own.
//!
//! A pool is a range of title addresses in 64 KiB pages. A handle is the pool's place in
//! the list of pools, counted from one; zero is no pool.

use std::sync::Mutex;

use crate::device::Device;
use crate::object::{E_INVALIDARG, Hresult, S_OK};
use crate::trace;

const PAGE: u64 = 64 << 10;

/// A registered pool: where it starts and how many pages it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pool {
    pub(crate) address: u64,
    pub(crate) pages: u32,
}

/// Every pool registered so far. One that was unregistered leaves its place empty, so the
/// handles of the others stay what they were.
static POOLS: Mutex<Vec<Option<Pool>>> = Mutex::new(Vec::new());

fn pools() -> std::sync::MutexGuard<'static, Vec<Option<Pool>>> {
    POOLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The pool a handle names.
#[allow(dead_code)] // the first user is copying page mappings on a queue
pub(crate) fn pool(handle: usize) -> Option<Pool> {
    pools().get(handle.checked_sub(1)?).copied().flatten()
}

/// `ID3D12Device::RegisterPagePoolX`: `(address, page count, handle out)`.
///
/// # Safety
/// `out` is null or where the handle goes.
pub(crate) unsafe extern "system" fn register(
    _device: &Device,
    address: u64,
    pages: u32,
    out: *mut usize,
) -> Hresult {
    if out.is_null() || pages == 0 || address == 0 || !address.is_multiple_of(PAGE) {
        return E_INVALIDARG;
    }
    let handle = {
        let mut pools = pools();
        pools.push(Some(Pool { address, pages }));
        pools.len()
    };
    trace!(
        "page pool {handle} registered: {pages} pages ({} MiB) at {address:#x}",
        (u64::from(pages) * PAGE) >> 20
    );
    // SAFETY: checked non-null.
    unsafe { out.write(handle) };
    S_OK
}

/// `ID3D12Device::UnregisterPagePoolX`: `(handle)`.
pub(crate) extern "system" fn unregister(_device: &Device, handle: usize) -> Hresult {
    let mut pools = pools();
    match handle.checked_sub(1).and_then(|index| pools.get_mut(index)) {
        Some(pool @ Some(_)) => {
            *pool = None;
            trace!("page pool {handle} unregistered");
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_keeps_its_handle_while_others_come_and_go() {
        let device = crate::device::device();
        let (mut first, mut second) = (0, 0);
        // SAFETY: valid out-pointers, and the null one under test.
        unsafe {
            assert_eq!(register(device, 0x1000_0000, 16, &mut first), S_OK);
            assert_eq!(register(device, 0x2000_0000, 32, &mut second), S_OK);
            assert_eq!(
                register(device, 0x2000_0000, 32, std::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(register(device, 0x2000_1000, 32, &mut first), E_INVALIDARG);
            assert_eq!(register(device, 0x2000_0000, 0, &mut first), E_INVALIDARG);
        }
        assert_ne!(first, 0);
        assert_ne!(first, second);
        assert_eq!(
            pool(second),
            Some(Pool {
                address: 0x2000_0000,
                pages: 32
            })
        );
        assert_eq!(unregister(device, first), S_OK);
        assert_eq!(unregister(device, first), E_INVALIDARG);
        assert_eq!(pool(first), None);
        assert_eq!(pool(second).map(|pool| pool.pages), Some(32));
        assert_eq!(pool(0), None);
        assert_eq!(unregister(device, 0), E_INVALIDARG);
    }
}
