//! Title memory as the host GPU sees it (decisions M1, M4 and M5).
//!
//! Each bank of title memory is opened once as a host heap with one buffer over all of it.
//! A range of title addresses that lies in one stretch of one bank is then a range of that
//! buffer, which the GPU reads and writes where it is.

use std::sync::Mutex;

use riptide_mem::InPlaceHeap;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, ID3D12Resource,
};

use crate::trace;

/// The banks opened so far, by number.
static BANKS: Mutex<Vec<Option<InPlaceHeap>>> = Mutex::new(Vec::new());

/// A range of title memory in a bank's buffer.
pub(crate) struct InPlace {
    /// The bank's buffer.
    #[allow(dead_code)] // the first users are copies and views
    pub(crate) buffer: ID3D12Resource,
    /// Where the range starts in it.
    #[allow(dead_code)] // the first users are copies and views
    pub(crate) offset: u64,
    /// The host GPU's address of the range's first byte.
    pub(crate) gpu_address: u64,
}

/// Where the host GPU finds `bytes` of title memory at `address`, if they are mapped and
/// lie in one stretch of one bank. `None` otherwise, and then the range can only be reached
/// through a copy.
pub(crate) fn find(address: u64, bytes: u64) -> Option<InPlace> {
    let services = crate::services()?;
    let parts = services.memory.locate(address, bytes)?;
    let [part] = parts.as_slice() else {
        return None;
    };
    let mut banks = BANKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if banks.len() <= part.bank {
        banks.resize_with(part.bank + 1, || None);
    }
    if banks[part.bank].is_none() {
        let host = crate::host::host().ok()?;
        // Unordered access is allowed so that compute passes can write title memory.
        match services.memory.open_heap(
            part.bank,
            &host.device,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ) {
            Ok(heap) => {
                trace!("bank {} of title memory opened for the GPU", part.bank);
                banks[part.bank] = Some(heap);
            }
            Err(error) => {
                trace!(
                    "bank {} of title memory could not be opened for the GPU: {error}",
                    part.bank
                );
                return None;
            }
        }
    }
    let buffer = banks[part.bank].as_ref()?.buffer.clone();
    // SAFETY: a plain call on the host buffer.
    let start = unsafe { buffer.GetGPUVirtualAddress() };
    Some(InPlace {
        buffer,
        offset: part.offset,
        gpu_address: start + part.offset,
    })
}
