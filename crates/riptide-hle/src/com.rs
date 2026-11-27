//! Console objects the title calls through a table of methods.
//!
//! Much of the console's API is handed to the title as objects: a pointer to a pointer to a
//! table of function addresses, called by position. An object made here has a table of the
//! interface's full length. The methods that are built go in their positions; every other
//! position holds a stub of its own that names the interface and the position when it is
//! called and stops the title, as an unbuilt import does. So a missing method is found by
//! name the first time a title needs it.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use crate::stub::Stubs;
use crate::trace::trace;

/// The stubs behind unbuilt methods. They live as long as the process.
static STUBS: Mutex<Option<Stubs>> = Mutex::new(None);

/// A stub that, when called, logs `name` as unimplemented and stops the title.
pub(crate) fn stub(name: String) -> usize {
    let mut stubs = STUBS.lock().unwrap_or_else(|poison| poison.into_inner());
    stubs
        .get_or_insert_with(|| {
            Stubs::new(Arc::new(|line: &str| {
                eprintln!("riptide: {line}");
                trace!("{line}");
            }))
        })
        .add(name)
        .expect("Windows gives memory for a stub")
}

/// What an object starts with: the address of its table. An object's own data follows it.
#[repr(C)]
pub(crate) struct Header {
    table: *const usize,
}

// SAFETY: the table is leaked, never written after it is made, and holds only addresses.
unsafe impl Send for Header {}
// SAFETY: as above.
unsafe impl Sync for Header {}

impl Header {
    /// A header whose table has `slots` positions: `methods` at the positions they name,
    /// stubs named `interface slot n` everywhere else. The table is never freed.
    pub(crate) fn new(interface: &str, slots: usize, methods: &[(usize, usize)]) -> Header {
        let mut table: Vec<usize> = (0..slots)
            .map(|slot| {
                methods
                    .iter()
                    .find(|&&(at, _)| at == slot)
                    .map_or(0, |&(_, method)| method)
            })
            .collect();
        for (slot, entry) in table.iter_mut().enumerate() {
            if *entry == 0 {
                *entry = stub(format!("{interface} slot {slot} (+{:#x})", slot * 8));
            }
        }
        Header {
            table: Box::leak(table.into_boxed_slice()).as_ptr(),
        }
    }
}

/// An object with data of its own, counted: the title holds references and the object goes
/// when the last one does. Its address is what the title is handed.
#[repr(C)]
pub(crate) struct Instance<T> {
    header: Header,
    references: std::sync::atomic::AtomicU32,
    pub(crate) state: T,
}

impl<T> Instance<T> {
    /// Makes an object with one reference, which the caller hands to the title. `table` is
    /// the interface's method table, made once with [`table`].
    pub(crate) fn create(table: &'static [usize], state: T) -> *mut Instance<T> {
        Box::into_raw(Box::new(Instance {
            header: Header {
                table: table.as_ptr(),
            },
            references: std::sync::atomic::AtomicU32::new(1),
            state,
        }))
    }

    /// `AddRef`.
    ///
    /// # Safety
    /// `this` is a live object made by [`Instance::create`] with this `T`.
    pub(crate) unsafe extern "system" fn add_ref(this: *mut Instance<T>) -> u32 {
        // SAFETY: as this function's contract says.
        unsafe { &*this }
            .references
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// `Release`: the object is freed with its last reference.
    ///
    /// # Safety
    /// `this` is a live object made by [`Instance::create`] with this `T`, and the caller
    /// gives up one reference to it.
    pub(crate) unsafe extern "system" fn release(this: *mut Instance<T>) -> u32 {
        // SAFETY: as this function's contract says.
        let left = unsafe { &*this }
            .references
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            - 1;
        if left == 0 {
            // SAFETY: that was the last reference, and the object came from `Box::into_raw`.
            drop(unsafe { Box::from_raw(this) });
        }
        left
    }
}

/// A method table of `slots` positions for `interface`: `methods` at the positions they
/// name, stubs that name themselves everywhere else. Made once per interface and never
/// freed.
pub(crate) fn table(interface: &str, slots: usize, methods: &[(usize, usize)]) -> &'static [usize] {
    let header = Header::new(interface, slots, methods);
    // SAFETY: `Header::new` leaked a table of exactly `slots` entries.
    unsafe { std::slice::from_raw_parts(header.table, slots) }
}

/// An object with no data of its own that lives as long as the process: the address to hand
/// the title.
pub(crate) fn singleton(interface: &str, slots: usize, methods: &[(usize, usize)]) -> usize {
    let header: &'static Header = Box::leak(Box::new(Header::new(interface, slots, methods)));
    std::ptr::from_ref(header) as usize
}

/// `AddRef` and `Release` for an object that is never freed: always one reference left.
pub(crate) extern "system" fn keep(_this: *mut c_void) -> u32 {
    1
}

/// Registers a callback for an event that never happens here: `(this, queue, context,
/// callback, token)`. The callback is never called; the registration gets a token like any
/// other, so the title can take it off again.
///
/// # Safety
/// `token` is null or where the registration's token goes.
pub(crate) unsafe extern "system" fn register_for_nothing(
    _this: *mut c_void,
    _queue: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    token: *mut u64,
) -> i32 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    if !token.is_null() {
        // SAFETY: checked non-null.
        unsafe { token.write(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) };
    }
    0
}

/// Takes a registration made by [`register_for_nothing`] off: `(this, token, wait)`.
pub(crate) extern "system" fn unregister_for_nothing(
    _this: *mut c_void,
    _token: u64,
    _wait: bool,
) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn seven(_this: *mut c_void) -> u32 {
        7
    }

    #[test]
    fn built_methods_sit_at_their_positions_and_the_rest_are_stubs() {
        let object = singleton("ITest", 4, &[(2, seven as *const () as usize)]);
        // SAFETY: the object starts with the address of its four-entry table.
        let table = unsafe { std::slice::from_raw_parts(*(object as *const *const usize), 4) };
        assert_eq!(table[2], seven as *const () as usize);
        // Each other position has a stub of its own.
        assert!(table[0] != 0 && table[1] != 0 && table[3] != 0);
        assert!(table[0] != table[1] && table[1] != table[3]);
        // SAFETY: position 2 holds `seven`, called as a method of the object.
        let method = unsafe {
            std::mem::transmute::<usize, extern "system" fn(*mut c_void) -> u32>(table[2])
        };
        assert_eq!(method(object as *mut c_void), 7);
    }
}
