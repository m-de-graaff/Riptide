//! What every console graphics object starts with, and the tables of methods behind them.
//!
//! A title holds a graphics object as a pointer to a pointer to a table of function
//! addresses, and calls by position. Some of a title's code is compiled against the object's
//! own layout and reads fields straight out of it, so the first 16 bytes are the console's:
//! the table, a word of bookkeeping bits, and the reference count.

use std::sync::atomic::{AtomicU32, Ordering};

/// A COM result code.
pub type Hresult = i32;

pub(crate) const S_OK: Hresult = 0;
pub(crate) const E_NOTIMPL: Hresult = 0x8000_4001_u32 as i32;
pub(crate) const E_NOINTERFACE: Hresult = 0x8000_4002_u32 as i32;
pub(crate) const E_POINTER: Hresult = 0x8000_4003_u32 as i32;
pub(crate) const E_INVALIDARG: Hresult = 0x8007_0057_u32 as i32;
pub(crate) const E_OUTOFMEMORY: Hresult = 0x8007_000e_u32 as i32;

/// An interface id, as it lies in memory.
pub(crate) type Iid = [u8; 16];

/// An interface id from the fields of its text form.
pub(crate) const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> Iid {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

/// An interface id as text, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
pub(crate) fn guid_text(id: &Iid) -> String {
    let a = u32::from_le_bytes([id[0], id[1], id[2], id[3]]);
    let b = u16::from_le_bytes([id[4], id[5]]);
    let c = u16::from_le_bytes([id[6], id[7]]);
    let d: String = id[8..].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{a:08x}-{b:04x}-{c:04x}-{}-{}", &d[..4], &d[4..])
}

/// `IUnknown`, which every object answers to.
pub(crate) const IUNKNOWN: Iid = guid(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);

/// Whether the id at `iid` is `IUnknown` or one of `known`.
///
/// # Safety
/// `iid` is null or points at an interface id.
pub(crate) unsafe fn is(iid: *const Iid, known: &[Iid]) -> bool {
    // SAFETY: the caller's pointer; null is not an interface.
    unsafe { iid.as_ref() }.is_some_and(|id| *id == IUNKNOWN || known.contains(id))
}

/// The first 16 bytes of every console graphics object.
#[repr(C)]
pub(crate) struct Object {
    table: *const usize,
    /// The console's bookkeeping bits (the device's index among them). All zero here.
    bits: u32,
    references: AtomicU32,
}

const _: () = assert!(size_of::<Object>() == 0x10);

// SAFETY: the table is leaked and never written after it is made; the count is atomic.
unsafe impl Send for Object {}
// SAFETY: as above.
unsafe impl Sync for Object {}

impl Object {
    /// A header with `references` references, on a table made by [`table`].
    pub(crate) fn new(table: &'static [usize], references: u32) -> Object {
        Object {
            table: table.as_ptr(),
            bits: 0,
            references: AtomicU32::new(references),
        }
    }

    pub(crate) fn add_ref(&self) -> u32 {
        self.references.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Takes one reference away and says how many are left. Never goes below zero.
    pub(crate) fn release(&self) -> u32 {
        let before = self
            .references
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            });
        before.map_or(0, |count| count - 1)
    }
}

/// The ids of the three interfaces every device child inherits: `ID3D12Pageable`,
/// `ID3D12DeviceChild` and `ID3D12Object`.
pub(crate) const CHILD_IIDS: [Iid; 3] = [
    guid(
        0x63ee58fb,
        0x1268,
        0x4835,
        [0x86, 0xda, 0xf0, 0x08, 0xce, 0x62, 0xf0, 0xd6],
    ),
    guid(
        0x905db94b,
        0xa00c,
        0x4140,
        [0x9d, 0xf5, 0x2b, 0x64, 0xca, 0x9e, 0xa3, 0x57],
    ),
    guid(
        0xc4fec28f,
        0x7966,
        0x4e95,
        [0x9f, 0x94, 0xf4, 0x31, 0xcb, 0x56, 0xc3, 0xb8],
    ),
];

/// An object made from the device: a queue, a fence, a list, a resource. It is counted and
/// goes with its last reference.
///
/// # Safety
/// The type is `#[repr(C)]` and its first field is its [`Object`], so a pointer to one is a
/// pointer to its header.
pub(crate) unsafe trait Child: Sized {
    /// What the session log calls it.
    const NAME: &'static str;
    /// The ids of its own interfaces. It also answers to `IUnknown` and [`CHILD_IIDS`].
    const IIDS: &'static [Iid];

    fn header(&self) -> &Object;
}

/// Whether the id at `iid` names a `T`.
///
/// # Safety
/// `iid` is null or points at an interface id.
unsafe fn names<T: Child>(iid: *const Iid) -> bool {
    // SAFETY: the caller's pointer.
    unsafe { is(iid, T::IIDS) || is(iid, &CHILD_IIDS) }
}

/// Whether a `T` can be handed out as the interface at `iid` through `out`: asked before
/// the object is made, so nothing is made for a request that is refused. On a refusal `out`
/// is set to null if there is one.
///
/// # Safety
/// `iid` is null or points at an interface id; `out` is null or where the object goes.
pub(crate) unsafe fn accepts<T: Child>(iid: *const Iid, out: *mut *const T) -> Result<(), Hresult> {
    if out.is_null() {
        return Err(E_POINTER);
    }
    // SAFETY: checked non-null.
    unsafe { out.write(std::ptr::null()) };
    // SAFETY: the caller's id.
    if !unsafe { names::<T>(iid) } {
        // SAFETY: the caller's id; null has no text.
        let id = unsafe { iid.as_ref() }.map(guid_text).unwrap_or_default();
        crate::trace!(
            "a {} was asked for as interface {id}, which it is not",
            T::NAME
        );
        return Err(E_NOINTERFACE);
    }
    Ok(())
}

/// Hands a new object to the title, after [`accepts`] said it can be. `child` has one
/// reference, which becomes the title's.
///
/// # Safety
/// `out` is where the object goes, as checked by [`accepts`].
pub(crate) unsafe fn hand_out<T: Child>(child: Box<T>, out: *mut *const T) -> Hresult {
    // SAFETY: the caller checked it.
    unsafe { out.write(Box::into_raw(child)) };
    S_OK
}

/// `QueryInterface` for a device child.
///
/// # Safety
/// `this` is a live `T`; `iid` and `out` are null or valid.
pub(crate) unsafe extern "system" fn query_interface<T: Child>(
    this: *const T,
    iid: *const Iid,
    out: *mut *const T,
) -> Hresult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the caller's id.
    let found = unsafe { names::<T>(iid) };
    if found {
        // SAFETY: a live object.
        unsafe { &*this }.header().add_ref();
    }
    // SAFETY: checked non-null.
    unsafe { out.write(if found { this } else { std::ptr::null() }) };
    if found { S_OK } else { E_NOINTERFACE }
}

/// `AddRef` for a device child.
pub(crate) extern "system" fn add_ref<T: Child>(this: &T) -> u32 {
    this.header().add_ref()
}

/// `Release` for a device child: it goes with its last reference.
///
/// # Safety
/// `this` is a live `T` handed out by [`hand_out`], and the caller gives up one reference.
pub(crate) unsafe extern "system" fn release<T: Child>(this: *mut T) -> u32 {
    // SAFETY: a live object.
    let left = unsafe { &*this }.header().release();
    if left == 0 {
        // SAFETY: that was the last reference, and the object came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

/// `GetDevice` for a device child: the device, as the interface asked for.
///
/// # Safety
/// `iid` and `out` are null or valid.
pub(crate) unsafe extern "system" fn get_device<T: Child>(
    _this: *const T,
    iid: *const Iid,
    out: *mut *const crate::device::Device,
) -> Hresult {
    // SAFETY: as received.
    unsafe { crate::device::answer(iid, out) }
}

/// The four methods every device child's table starts with and has at position 7.
pub(crate) fn child_methods<T: Child>() -> [(usize, usize); 4] {
    [
        (0, query_interface::<T> as *const () as usize),
        (1, add_ref::<T> as *const () as usize),
        (2, release::<T> as *const () as usize),
        (7, get_device::<T> as *const () as usize),
    ]
}

/// Which of a method's first two arguments is the object, and which the pointer for its
/// result. A method that returns a structure takes a pointer for it, and which side of the
/// object that is on differs from method to method; the object is the one that starts with
/// its own table.
///
/// # Safety
/// Each of `first` and `second` is null or points at 8 readable bytes, and one of them is a
/// live `T` made on `table`.
pub(crate) unsafe fn this_and_result<'a, T: Child, R>(
    table: &'static [usize],
    first: *const T,
    second: *const T,
) -> (&'a T, *mut R) {
    // SAFETY: readable, as the contract says.
    let first_is_object = !first.is_null()
        && unsafe { first.cast::<usize>().read_unaligned() } == table.as_ptr() as usize;
    if first_is_object {
        // SAFETY: it starts with the table, so it is the object.
        (unsafe { &*first }, second.cast_mut().cast())
    } else {
        // SAFETY: the other one is the object.
        (unsafe { &*second }, first.cast_mut().cast())
    }
}

/// What an unbuilt method is when this crate has not been given a stub maker (in its own
/// tests): it fails, as an interface without the method would.
extern "system" fn unbuilt() -> Hresult {
    E_NOTIMPL
}

/// A table of `slots` methods for `interface`: `methods` at the positions they name, and at
/// every other position a stub that names the interface and the method when called and
/// stops the title. `names` gives the methods' names by position, as far as they are known.
/// The table is made once per interface and never freed.
pub(crate) fn table(
    interface: &str,
    slots: usize,
    names: &[(usize, &str)],
    methods: &[(usize, usize)],
) -> &'static [usize] {
    let table: Vec<usize> = (0..slots)
        .map(|slot| {
            if let Some(&(_, method)) = methods.iter().find(|&&(at, _)| at == slot) {
                return method;
            }
            let Some(services) = crate::services() else {
                return unbuilt as *const () as usize;
            };
            let offset = slot * 8;
            let name = match names.iter().find(|&&(at, _)| at == slot) {
                Some(&(_, name)) => format!("{interface}::{name} (slot {slot}, +{offset:#x})"),
                None => format!("{interface} slot {slot} (+{offset:#x})"),
            };
            (services.stub)(name)
        })
        .collect();
    Box::leak(table.into_boxed_slice())
}

/// The method at `slot` of the object at `this`, as a function of type `F`.
///
/// # Safety
/// `this` is a live object whose table has that slot, and `F` is a function pointer type
/// matching the method there.
#[cfg(test)]
pub(crate) unsafe fn method<F: Copy>(this: *const Object, slot: usize) -> F {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    // SAFETY: as this function's contract says.
    unsafe { std::mem::transmute_copy(&*(*this).table.add(slot)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interface_id_reads_as_its_text() {
        let id = guid(
            0x189819f1,
            0x1db6,
            0x4b57,
            [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
        );
        assert_eq!(guid_text(&id), "189819f1-1db6-4b57-be54-1821339b85f7");
        assert_eq!(id[..4], [0xf1, 0x19, 0x98, 0x18]);
    }

    #[test]
    fn every_object_answers_to_iunknown_and_to_its_own_ids() {
        let own = [[7u8; 16]];
        // SAFETY: valid ids, and null.
        unsafe {
            assert!(is(&IUNKNOWN, &own));
            assert!(is(&own[0], &own));
            assert!(!is(&[8u8; 16], &own));
            assert!(!is(std::ptr::null(), &own));
        }
    }

    #[test]
    fn a_reference_count_goes_up_and_down_and_stops_at_zero() {
        let object = Object::new(table("test", 3, &[], &[]), 1);
        assert_eq!(object.add_ref(), 2);
        assert_eq!(object.release(), 1);
        assert_eq!(object.release(), 0);
        assert_eq!(object.release(), 0);
    }

    #[test]
    fn a_table_holds_its_methods_and_fails_everywhere_else() {
        extern "system" fn seven() -> Hresult {
            7
        }
        let made = table(
            "test",
            4,
            &[(1, "Named")],
            &[(2, seven as *const () as usize)],
        );
        assert_eq!(made.len(), 4);
        let object = Object::new(made, 1);
        // SAFETY: the table has four slots of this shape.
        unsafe {
            let built: extern "system" fn() -> Hresult = method(&object, 2);
            assert_eq!(built(), 7);
            let missing: extern "system" fn() -> Hresult = method(&object, 1);
            assert_eq!(missing(), E_NOTIMPL);
        }
    }
}
