//! The runtime's user area (`XUser`): who is signed in.
//!
//! Built so far: only what the real title has called. The plan is one local user who is
//! always signed in.

/// The area's provider id, which is also the interface id a title asks for:
/// `01acd177-91f9-4763-a38e-ccbb55ce32e0`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x77, 0xd1, 0xac, 0x01, 0xf9, 0x91, 0x63, 0x47, 0xa3, 0x8e, 0xcc, 0xbb, 0x55, 0xce, 0x32, 0xe0,
];

pub(crate) const NAME: &str = "users";

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        // +0x108 and +0x110 `XUserRegisterForChangeEvent`: told when a user signs in or out,
        // which never happens here.
        (33, crate::com::register_for_nothing as *const () as usize),
        (34, crate::com::unregister_for_nothing as *const () as usize),
    ]
}
