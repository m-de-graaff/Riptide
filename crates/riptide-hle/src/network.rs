//! The runtime's networking area (`XNetworking`), answered as a console with no connection:
//! the network stack is up, which titles wait for before anything else, and it reaches
//! nothing.

use std::ffi::c_void;

/// The interface id a title asks for: `bf2346b2-39af-4658-b5ea-44713c7e83b3`.
pub(crate) const INTERFACE: [u8; 16] = [
    0xb2, 0x46, 0x23, 0xbf, 0xaf, 0x39, 0x58, 0x46, 0xb5, 0xea, 0x44, 0x71, 0x3c, 0x7e, 0x83, 0xb3,
];

pub(crate) const NAME: &str = "networking";

const S_OK: i32 = 0;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;

/// `XNetworkingConnectivityHint`.
#[repr(C)]
pub struct ConnectivityHint {
    /// 0 unknown, 1 none, 2 local access, 3 internet access, 4 constrained internet access.
    level: u32,
    /// 0 unknown, 1 unrestricted, 2 fixed, 3 variable.
    cost: u32,
    /// The kind of network interface, as IANA numbers them. 6 is Ethernet.
    interface_type: u32,
    network_initialized: bool,
    approaching_data_limit: bool,
    over_data_limit: bool,
    roaming: bool,
}

const _: () = assert!(size_of::<ConnectivityHint>() == 16);

/// +0x78 `XNetworkingGetConnectivityHint`: up, wired, and connected to nothing.
///
/// # Safety
/// `out` is null or where the hint goes.
unsafe extern "system" fn connectivity_hint(_this: *mut c_void, out: *mut ConnectivityHint) -> i32 {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null; the title's own structure.
    unsafe {
        out.write(ConnectivityHint {
            level: 1,
            cost: 0,
            interface_type: 6,
            network_initialized: true,
            approaching_data_limit: false,
            over_data_limit: false,
            roaming: false,
        });
    }
    S_OK
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (15, connectivity_hint as *const () as usize),
        // +0x80 and +0x88: told when the hint changes, which it never does here.
        (16, crate::com::register_for_nothing as *const () as usize),
        (17, crate::com::unregister_for_nothing as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_network_is_up_and_reaches_nothing() {
        let mut hint = std::mem::MaybeUninit::<ConnectivityHint>::uninit();
        // SAFETY: a valid out-pointer, read only after the call filled it.
        let hint = unsafe {
            assert_eq!(
                connectivity_hint(std::ptr::null_mut(), hint.as_mut_ptr()),
                S_OK
            );
            hint.assume_init()
        };
        assert!(hint.network_initialized);
        assert_eq!(hint.level, 1);
        // SAFETY: a null out-pointer is refused, not written.
        let refused = unsafe { connectivity_hint(std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(refused, E_POINTER);
    }
}
