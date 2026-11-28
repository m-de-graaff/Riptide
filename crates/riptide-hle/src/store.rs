//! The runtime's store area: what the player owns.
//!
//! Riptide runs titles the player has, without a store to ask: the title is licensed in
//! full, for good, to the one local user, and there are no add-ons. Nothing here reaches
//! a network.
//!
//! Built so far: a store context, the game's licence, a licence for one of the title's own
//! packages, the add-on licences (none), a licence for a durable add-on (which is not
//! owned: Riptide cannot know what the player bought, and says nothing was), and callbacks
//! for a licence changing or being lost, which are registered and never called since
//! neither happens. Products, purchases,
//! updates and the store's own screens name themselves when called.

use std::ffi::c_void;

use crate::trace::trace;
use crate::xasync::{self, AsyncBlock};

/// The interface id a title asks for: `b09d803c-2414-4a05-82c6-66dfdc9e9a44`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x3c, 0x80, 0x9d, 0xb0, 0x14, 0x24, 0x05, 0x4a, 0x82, 0xc6, 0x66, 0xdf, 0xdc, 0x9e, 0x9a, 0x44,
];

/// The id of the same area as later consoles' titles ask for it, with methods added after
/// the first's: `2d42fea5-e71d-4b76-97cd-c50afbb3ae5d`.
pub(crate) const LATER_INTERFACE: [u8; 16] = [
    0xa5, 0xfe, 0x42, 0x2d, 0x1d, 0xe7, 0x76, 0x4b, 0x97, 0xcd, 0xc5, 0x0a, 0xfb, 0xb3, 0xae, 0x5d,
];

pub(crate) const NAME: &str = "store";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

/// What a store context's handle points at, what a licence's does, and what the handle of
/// a licence the player does not hold does. Only the addresses matter: there is one store,
/// and a licence is held or it is not.
static CONTEXT: u8 = 0;
static LICENSE: u8 = 1;
static NO_LICENSE: u8 = 2;

fn context() -> usize {
    &raw const CONTEXT as usize
}

fn license() -> usize {
    &raw const LICENSE as usize
}

fn no_license() -> usize {
    &raw const NO_LICENSE as usize
}

/// The game's licence, in the layout the title reads.
#[repr(C)]
struct GameLicense {
    /// The store's id for what was bought, as text. Riptide knows none.
    sku: [u8; 18],
    active: bool,
    trial_owned_by_this_user: bool,
    disc: bool,
    trial: bool,
    trial_seconds_left: u32,
    trial_id: [u8; 37],
    /// When the licence ends, in seconds since 1970.
    expires: i64,
}

const _: () = assert!(size_of::<GameLicense>() == 80);
const _: () = assert!(std::mem::offset_of!(GameLicense, expires) == 72);

/// The last second of the year 9999, which is how a licence without an end is dated.
const NEVER: i64 = 253_402_300_799;

/// The licence every title has here: the whole game, for good.
fn game_license() -> GameLicense {
    GameLicense {
        sku: [0; 18],
        active: true,
        trial_owned_by_this_user: false,
        disc: false,
        trial: false,
        trial_seconds_left: 0,
        trial_id: [0; 37],
        expires: NEVER,
    }
}

/// +0x18 `XStoreCreateContext`: `(user, context out)`.
///
/// # Safety
/// `out` is null or where the handle goes.
unsafe extern "system" fn create_context(_this: usize, _user: usize, out: *mut usize) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    *out = context();
    trace!("XStoreCreateContext: the title is licensed in full; no store is asked");
    S_OK
}

/// +0x20 `XStoreCloseContextHandle` and +0xb8 `XStoreCloseLicenseHandle`: `(handle)`.
/// Neither handle holds anything to let go of.
extern "system" fn close(_this: usize, _handle: usize) {}

/// What names the result of each call that has one: the call's own address.
fn game_license_identity() -> usize {
    query_game_license as *const () as usize
}

fn package_license_identity() -> usize {
    acquire_license_for_package as *const () as usize
}

fn durables_identity() -> usize {
    acquire_license_for_durables as *const () as usize
}

fn add_ons_identity() -> usize {
    query_add_on_licenses as *const () as usize
}

/// +0xe0 `XStoreQueryGameLicenseAsync`: `(context, block)`. Answered at once.
///
/// # Safety
/// `block` is null or a block that stays valid until the result is taken.
unsafe extern "system" fn query_game_license(
    _this: usize,
    _context: usize,
    block: *mut AsyncBlock,
) -> i32 {
    trace!("XStoreQueryGameLicenseAsync: active, not a trial, without an end");
    let license = game_license();
    // SAFETY: the licence is plain bytes of its own size.
    let bytes = unsafe {
        std::slice::from_raw_parts((&raw const license).cast::<u8>(), size_of::<GameLicense>())
    }
    .to_vec();
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, game_license_identity(), Ok(bytes)) }
}

/// +0xe8 `XStoreQueryGameLicenseResult`: `(block, licence out)`.
///
/// # Safety
/// `block` is null or the block of a game-licence call; `out` is null or where the
/// licence goes.
unsafe extern "system" fn game_license_result(
    _this: usize,
    block: *mut AsyncBlock,
    out: *mut c_void,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's block, and room for one licence.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            game_license_identity() as *const c_void,
            size_of::<GameLicense>(),
            out,
            std::ptr::null_mut(),
        )
    }
}

/// +0xa0 `XStoreAcquireLicenseForPackageAsync`: `(context, package, block)`. Every package
/// the title has is the player's.
///
/// # Safety
/// `block` is null or a block that stays valid until the result is taken.
unsafe extern "system" fn acquire_license_for_package(
    _this: usize,
    _context: usize,
    _package: *const u8,
    block: *mut AsyncBlock,
) -> i32 {
    trace!("XStoreAcquireLicenseForPackageAsync: licensed");
    let bytes = license().to_ne_bytes().to_vec();
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, package_license_identity(), Ok(bytes)) }
}

/// +0xa8 `XStoreAcquireLicenseForPackageResult`: `(block, licence handle out)`.
///
/// # Safety
/// `block` is null or the block of a package-licence call; `out` is null or where the
/// handle goes.
unsafe extern "system" fn package_license_result(
    _this: usize,
    block: *mut AsyncBlock,
    out: *mut usize,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's block, and room for one handle.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            package_license_identity() as *const c_void,
            size_of::<usize>(),
            out.cast(),
            std::ptr::null_mut(),
        )
    }
}

/// +0xb0 `XStoreIsLicenseValid`: `(licence)`. The licence of the game and its packages is;
/// the one handed out for what is not owned is not.
extern "system" fn is_license_valid(_this: usize, asked: usize) -> bool {
    asked == license()
}

/// +0x238 `XStoreAcquireLicenseForDurablesAsync`: `(context, store id, block)`. The
/// add-on is not owned: the title gets a licence that is not valid.
///
/// # Safety
/// `id` is null or terminated text, and `block` is null or a block that stays valid until
/// the result is taken.
unsafe extern "system" fn acquire_license_for_durables(
    _this: usize,
    _context: usize,
    id: *const std::ffi::c_char,
    block: *mut AsyncBlock,
) -> i32 {
    let id = if id.is_null() {
        String::new()
    } else {
        // SAFETY: the title's terminated text.
        unsafe { std::ffi::CStr::from_ptr(id) }
            .to_string_lossy()
            .into_owned()
    };
    trace!("XStoreAcquireLicenseForDurablesAsync({id}): not owned");
    let bytes = no_license().to_ne_bytes().to_vec();
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, durables_identity(), Ok(bytes)) }
}

/// +0x240 `XStoreAcquireLicenseForDurablesResult`: `(block, licence handle out)`.
///
/// # Safety
/// `block` is null or the block of a durables-licence call; `out` is null or where the
/// handle goes.
unsafe extern "system" fn durables_license_result(
    _this: usize,
    block: *mut AsyncBlock,
    out: *mut usize,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's block, and room for one handle.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            durables_identity() as *const c_void,
            size_of::<usize>(),
            out.cast(),
            std::ptr::null_mut(),
        )
    }
}

/// +0xf0 `XStoreQueryAddOnLicensesAsync`: `(context, block)`. There are none.
///
/// # Safety
/// `block` is null or a block that stays valid until the result is taken.
unsafe extern "system" fn query_add_on_licenses(
    _this: usize,
    _context: usize,
    block: *mut AsyncBlock,
) -> i32 {
    trace!("XStoreQueryAddOnLicensesAsync: none");
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, add_ons_identity(), Ok(Vec::new())) }
}

/// +0xf8 `XStoreQueryAddOnLicensesResultCount`: `(block, count out)`.
///
/// # Safety
/// `count` is null or where the count goes.
unsafe extern "system" fn add_on_licenses_count(
    _this: usize,
    _block: *mut AsyncBlock,
    count: *mut u32,
) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(count) = (unsafe { count.as_mut() }) else {
        return E_INVALIDARG;
    };
    *count = 0;
    S_OK
}

/// +0x100 `XStoreQueryAddOnLicensesResult`: `(block, count, licences out)`. Nothing is
/// written, and the call is over.
///
/// # Safety
/// `block` is null or the block of an add-on-licences call.
unsafe extern "system" fn add_on_licenses_result(
    _this: usize,
    block: *mut AsyncBlock,
    _count: u32,
    _out: *mut c_void,
) -> i32 {
    // SAFETY: the title's block; an empty result needs no room.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            add_ons_identity() as *const c_void,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }
}

/// +0x210 `XStoreRegisterGameLicenseChanged` and +0x220 `XStoreRegisterPackageLicenseLost`:
/// `(context or licence, queue, callback context, callback, token out)`. A licence here
/// neither changes nor is lost, so the callback is never called.
///
/// # Safety
/// `token` is null or where the registration's token goes.
unsafe extern "system" fn register(
    _this: usize,
    _handle: usize,
    _queue: usize,
    _context: usize,
    _callback: usize,
    token: *mut u64,
) -> i32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    // SAFETY: the title's out-pointer, null or writable.
    if let Some(token) = unsafe { token.as_mut() } {
        *token = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    S_OK
}

/// +0x218 `XStoreUnregisterGameLicenseChanged` and +0x228
/// `XStoreUnregisterPackageLicenseLost`: `(context or licence, token, wait)`.
extern "system" fn unregister(_this: usize, _handle: usize, _token: u64, _wait: bool) -> bool {
    true
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (3, create_context as *const () as usize),
        (4, close as *const () as usize),
        (20, acquire_license_for_package as *const () as usize),
        (21, package_license_result as *const () as usize),
        (22, is_license_valid as *const () as usize),
        (23, close as *const () as usize),
        (28, query_game_license as *const () as usize),
        (29, game_license_result as *const () as usize),
        (30, query_add_on_licenses as *const () as usize),
        (31, add_on_licenses_count as *const () as usize),
        (32, add_on_licenses_result as *const () as usize),
        (66, register as *const () as usize),
        (67, unregister as *const () as usize),
        (68, register as *const () as usize),
        (69, unregister as *const () as usize),
        (71, acquire_license_for_durables as *const () as usize),
        (72, durables_license_result as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_game_is_licensed_in_full_and_for_good() {
        let license = game_license();
        assert!(license.active && !license.trial && !license.trial_owned_by_this_user);
        assert_eq!(license.expires, NEVER);
        assert_eq!(license.trial_seconds_left, 0);
    }

    #[test]
    fn a_context_is_made_and_a_missing_place_for_it_is_refused() {
        let mut handle = 0usize;
        // SAFETY: called as a title calls it, with a pointer to a local and with null.
        unsafe {
            assert_eq!(create_context(0, 0, &mut handle), S_OK);
            assert_eq!(handle, context());
            assert_eq!(create_context(0, 0, std::ptr::null_mut()), E_INVALIDARG);
        }
        assert!(is_license_valid(0, license()));
        // What is handed out for an add-on that is not owned is no valid licence.
        assert!(!is_license_valid(0, no_license()));
    }

    #[test]
    fn there_are_no_add_ons_and_registrations_get_tokens_of_their_own() {
        let (mut count, mut one, mut other) = (7u32, 0u64, 0u64);
        // SAFETY: called as a title calls them, with pointers to locals.
        unsafe {
            assert_eq!(
                add_on_licenses_count(0, std::ptr::null_mut(), &mut count),
                S_OK
            );
            assert_eq!(register(0, context(), 0, 0, 0, &mut one), S_OK);
            assert_eq!(register(0, license(), 0, 0, 0, &mut other), S_OK);
        }
        assert_eq!(count, 0);
        assert!(one != 0 && other != 0 && one != other);
        assert!(unregister(0, context(), one, true));
    }

    #[test]
    fn each_built_method_has_a_position_of_its_own() {
        let mut slots: Vec<usize> = methods().iter().map(|&(slot, _)| slot).collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), methods().len());
    }
}
