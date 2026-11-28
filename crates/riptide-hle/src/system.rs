//! The runtime's system area: what console the title is on, as far as its identity goes.
//!
//! Built so far: the name of the service sandbox the console is in, and the hint that the
//! title may use all of the download bandwidth. The console's own id and the id of this
//! device for this title name themselves when called: an id has to be made up and kept,
//! and nothing has needed one yet.

use crate::package::copy_text;
use crate::trace::trace;

/// The interface id a title asks for: `1861cf2e-e18b-4834-a9f5-b4a4e6efb4cf`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x2e, 0xcf, 0x61, 0x18, 0x8b, 0xe1, 0x34, 0x48, 0xa9, 0xf5, 0xb4, 0xa4, 0xe6, 0xef, 0xb4, 0xcf,
];

pub(crate) const NAME: &str = "system";

#[cfg(test)]
const S_OK: i32 = 0;

/// The sandbox every console a player buys is in.
const SANDBOX: &str = "RETAIL";

/// `XSystemGetXboxLiveSandboxId`: `(buffer size, buffer, bytes used out)`.
///
/// # Safety
/// `buffer` is null or has room for `size` bytes; `used` is null or where the count goes.
unsafe extern "system" fn sandbox_id(
    _this: usize,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> i32 {
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(used) = unsafe { used.as_mut() } {
        *used = SANDBOX.len() + 1;
    }
    // SAFETY: the title's buffer, as it describes it.
    let result = unsafe { copy_text(SANDBOX, size, buffer) };
    trace!(
        "XSystemGetXboxLiveSandboxId = {SANDBOX} ({:#010x})",
        result as u32
    );
    result
}

/// `XSystemAllowFullDownloadBandwidth`: `(allow)`. A hint to the console's downloads, of
/// which there are none here.
extern "system" fn allow_full_download_bandwidth(_this: usize, _allow: bool) {}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        // +0x20 `XSystemGetXboxLiveSandboxId`.
        (4, sandbox_id as *const () as usize),
        // +0x40 `XSystemAllowFullDownloadBandwidth`.
        (8, allow_full_download_bandwidth as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_is_retail_and_needs_room_for_its_name() {
        let mut buffer = [0xffu8; 16];
        let mut used = 0;
        // SAFETY: a buffer of the size given, and a valid count.
        unsafe {
            assert_eq!(sandbox_id(0, 16, buffer.as_mut_ptr(), &mut used), S_OK);
            assert_eq!(&buffer[..7], b"RETAIL\0");
            assert_eq!(used, 7);
            assert!(sandbox_id(0, 3, buffer.as_mut_ptr(), std::ptr::null_mut()) < 0);
            assert!(sandbox_id(0, 16, std::ptr::null_mut(), &mut used) < 0);
        }
    }
}
