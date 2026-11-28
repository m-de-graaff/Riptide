//! The runtime's user area (`XUser`): who is playing.
//!
//! There is one user, signed in on this machine and with no connection to the service, as
//! on a console that is offline. The user's id is made up and the same on every machine;
//! it names no real account. Adding a user gives that user, at once and with no prompt.
//!
//! Built: adding the user and taking the result, handles, the count of users, the local
//! id, the id, the guest and sign-in state, and registration for sign-in changes, which
//! never happen. Privileges, tokens, pictures and the rest name themselves when called.

use std::ffi::c_void;

use crate::trace::trace;
use crate::xasync::{self, AsyncBlock};

/// The interface id a title asks for: `01acd177-91f9-4763-a38e-ccbb55ce32e0`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x77, 0xd1, 0xac, 0x01, 0xf9, 0x91, 0x63, 0x47, 0xa3, 0x8e, 0xcc, 0xbb, 0x55, 0xce, 0x32, 0xe0,
];

pub(crate) const NAME: &str = "users";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
/// What the console answers for a user it does not know.
const E_GAMEUSER_USER_NOT_FOUND: i32 = 0x8924_5105_u32 as i32;

/// The one user. A handle is this object's address.
struct User {
    /// The id the service knows a user by. Made up: it names no real account.
    id: u64,
    /// The id this machine knows the user by while it runs.
    local_id: u64,
}

static USER: User = User {
    id: 0x0009_0000_0000_0001,
    local_id: 1,
};

fn handle() -> *const User {
    &raw const USER
}

/// What names an add-user call's result: this module's own address for it.
fn add_identity() -> usize {
    add_async as *const () as usize
}

/// +0x38 `XUserAddAsync`: `(options, block)`. The user is added at once.
///
/// # Safety
/// `block` is null or a block that stays valid until the result is taken.
unsafe extern "system" fn add_async(_this: usize, options: u32, block: *mut AsyncBlock) -> i32 {
    trace!("XUserAddAsync(options {options:#x}): the one local user, offline");
    let bytes = (handle() as usize).to_ne_bytes().to_vec();
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, add_identity(), Ok(bytes)) }
}

/// +0x40 `XUserAddResult`: `(block, user handle out)`.
///
/// # Safety
/// `block` is null or the block of an add-user call; `out` is null or where the handle goes.
unsafe extern "system" fn add_result(
    _this: usize,
    block: *mut AsyncBlock,
    out: *mut *const User,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's block, and room for one handle.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            add_identity() as *const c_void,
            size_of::<*const User>(),
            out.cast(),
            std::ptr::null_mut(),
        )
    }
}

/// +0x18 `XUserDuplicateHandle`: `(user, handle out)`. The user lives as long as the
/// process, so every handle is the same.
///
/// # Safety
/// `out` is null or where the handle goes.
unsafe extern "system" fn duplicate(_this: usize, user: *const User, out: *mut *const User) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    if user != handle() {
        return E_GAMEUSER_USER_NOT_FOUND;
    }
    *out = user;
    S_OK
}

/// +0x20 `XUserCloseHandle`.
extern "system" fn close(_this: usize, _user: *const User) {}

/// +0x28 `XUserCompare`: `(one user, another)`: zero if they are the same user.
extern "system" fn compare(_this: usize, one: *const User, other: *const User) -> i32 {
    i32::from(one != other)
}

/// +0x30 `XUserGetMaxUsers`: `(count out)`.
///
/// # Safety
/// `out` is null or where the count goes.
unsafe extern "system" fn max_users(_this: usize, out: *mut u32) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    *out = 1;
    S_OK
}

/// Writes one fact about the user, if `user` is the user and `out` is there.
///
/// # Safety
/// `out` is null or where the value goes.
unsafe fn tell<T>(user: *const User, out: *mut T, value: T) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    if user != handle() {
        return E_GAMEUSER_USER_NOT_FOUND;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(value) };
    S_OK
}

/// +0x48 `XUserGetLocalId`: `(user, local id out)`.
///
/// # Safety
/// `out` is null or where the id goes.
unsafe extern "system" fn local_id(_this: usize, user: *const User, out: *mut u64) -> i32 {
    // SAFETY: as received.
    unsafe { tell(user, out, USER.local_id) }
}

/// +0x50 `XUserFindUserByLocalId`: `(local id, user handle out)`.
///
/// # Safety
/// `out` is null or where the handle goes.
unsafe extern "system" fn find_by_local_id(_this: usize, id: u64, out: *mut *const User) -> i32 {
    if id != USER.local_id {
        return E_GAMEUSER_USER_NOT_FOUND;
    }
    // SAFETY: as received.
    unsafe { tell(handle(), out, handle()) }
}

/// +0x58 `XUserGetId`: `(user, id out)`.
///
/// # Safety
/// `out` is null or where the id goes.
unsafe extern "system" fn id(_this: usize, user: *const User, out: *mut u64) -> i32 {
    // SAFETY: as received.
    unsafe { tell(user, out, USER.id) }
}

/// +0x60 `XUserFindUserById`: `(id, user handle out)`.
///
/// # Safety
/// `out` is null or where the handle goes.
unsafe extern "system" fn find_by_id(_this: usize, id: u64, out: *mut *const User) -> i32 {
    if id != USER.id {
        return E_GAMEUSER_USER_NOT_FOUND;
    }
    // SAFETY: as received.
    unsafe { tell(handle(), out, handle()) }
}

/// +0x68 `XUserGetIsGuest`: `(user, whether a guest out)`.
///
/// # Safety
/// `out` is null or where the answer goes.
unsafe extern "system" fn is_guest(_this: usize, user: *const User, out: *mut bool) -> i32 {
    // SAFETY: as received.
    unsafe { tell(user, out, false) }
}

/// +0x70 `XUserGetState`: `(user, state out)`. Zero is signed in.
///
/// # Safety
/// `out` is null or where the state goes.
unsafe extern "system" fn state(_this: usize, user: *const User, out: *mut u32) -> i32 {
    // SAFETY: as received.
    unsafe { tell(user, out, 0) }
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (3, duplicate as *const () as usize),
        (4, close as *const () as usize),
        (5, compare as *const () as usize),
        (6, max_users as *const () as usize),
        (7, add_async as *const () as usize),
        (8, add_result as *const () as usize),
        (9, local_id as *const () as usize),
        (10, find_by_local_id as *const () as usize),
        (11, id as *const () as usize),
        (12, find_by_id as *const () as usize),
        (13, is_guest as *const () as usize),
        (14, state as *const () as usize),
        // +0x108 and +0x110 `XUserRegisterForChangeEvent`: told when a user signs in or out,
        // which never happens here.
        (33, crate::com::register_for_nothing as *const () as usize),
        (34, crate::com::unregister_for_nothing as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn adding_a_user_gives_the_one_user_who_is_signed_in_and_no_guest() {
        // A block with no completion routine: zeroes are a valid start.
        let mut block: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
        let block = block.as_mut_ptr().cast::<AsyncBlock>();
        let mut user = std::ptr::null();
        let (mut service_id, mut machine_id, mut guest, mut signed) = (0, 0, true, 9);
        let mut count = 0;
        let mut found = std::ptr::null();
        // SAFETY: a live block, and valid out-pointers.
        unsafe {
            assert_eq!(add_async(0, 0, block), S_OK);
            assert_eq!(add_result(0, block, &mut user), S_OK);
            assert_eq!(user, handle());
            assert_eq!(id(0, user, &mut service_id), S_OK);
            assert_eq!(local_id(0, user, &mut machine_id), S_OK);
            assert_eq!(is_guest(0, user, &mut guest), S_OK);
            assert_eq!(state(0, user, &mut signed), S_OK);
            assert_eq!(max_users(0, &mut count), S_OK);
            assert_eq!(find_by_local_id(0, machine_id, &mut found), S_OK);
            assert_eq!(
                find_by_id(0, service_id + 1, &mut found),
                E_GAMEUSER_USER_NOT_FOUND
            );
            // A handle that is no user's is not found, and a missing place is refused.
            assert_eq!(
                id(0, std::ptr::dangling(), &mut service_id),
                E_GAMEUSER_USER_NOT_FOUND
            );
            assert_eq!(id(0, user, std::ptr::null_mut()), E_INVALIDARG);
        }
        assert_eq!((guest, signed, count), (false, 0, 1));
        assert_eq!(found, handle());
        assert_eq!(compare(0, user, found), 0);
        assert_ne!(compare(0, user, std::ptr::null()), 0);
    }
}
