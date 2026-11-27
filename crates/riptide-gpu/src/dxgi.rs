//! The device's DXGI side: what a title walks to find the screen it draws for.
//!
//! A title asks its graphics device for a DXGI device, that for its adapter, and the adapter
//! for its first output, which it then names when it sets its frame interval. There is one
//! of each, as on the console, and each lives as long as the process. Their tables are in
//! the PC interfaces' order.

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::object::{self, E_INVALIDARG, E_NOINTERFACE, E_POINTER, Hresult, Iid, Object, S_OK};
use crate::object::{guid, guid_text};
use crate::trace;

/// `DXGI_ERROR_NOT_FOUND`: what an enumeration answers past its last item.
pub(crate) const NOT_FOUND: Hresult = 0x887a_0002_u32 as i32;

/// `IDXGIObject`, which all four answer to.
const OBJECT_IID: Iid = guid(
    0xaec22fb8,
    0x76f3,
    0x4639,
    [0x9b, 0xe0, 0x28, 0xeb, 0x43, 0xa6, 0x7a, 0x2e],
);

/// `IDXGIDevice`, `IDXGIDevice1` and `IDXGIDevice2`.
pub(crate) const DEVICE_IIDS: [Iid; 3] = [
    guid(
        0x54ec77fa,
        0x1377,
        0x44e6,
        [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c],
    ),
    guid(
        0x77db970f,
        0x6276,
        0x48ba,
        [0xba, 0x28, 0x07, 0x01, 0x43, 0xb4, 0x39, 0x2c],
    ),
    guid(
        0x05008617,
        0xfbfd,
        0x4051,
        [0xa7, 0x90, 0x14, 0x48, 0x84, 0xb4, 0xf6, 0xa9],
    ),
];

/// `IDXGIAdapter`, `IDXGIAdapter1` and `IDXGIAdapter2`.
const ADAPTER_IIDS: [Iid; 3] = [
    guid(
        0x2411e7e1,
        0x12ac,
        0x4ccf,
        [0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0],
    ),
    guid(
        0x29038f61,
        0x3839,
        0x4626,
        [0x91, 0xfd, 0x08, 0x68, 0x79, 0x01, 0x1a, 0x05],
    ),
    guid(
        0x0aa1ae0a,
        0xfa0e,
        0x4b84,
        [0x86, 0x44, 0xe0, 0x5f, 0xf8, 0xe5, 0xac, 0xb5],
    ),
];

/// `IDXGIFactory`, `IDXGIFactory1` and `IDXGIFactory2`.
const FACTORY_IIDS: [Iid; 3] = [
    guid(
        0x7b7166ec,
        0x21c7,
        0x44ae,
        [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69],
    ),
    guid(
        0x770aae78,
        0xf26f,
        0x4dba,
        [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
    ),
    guid(
        0x50c83a1c,
        0xe072,
        0x4c48,
        [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0],
    ),
];

/// `IDXGIOutput`.
const OUTPUT_IIDS: [Iid; 1] = [guid(
    0xae02eedb,
    0xc735,
    0x4690,
    [0x8d, 0x52, 0x5a, 0x8d, 0xc2, 0x02, 0x13, 0xaa],
)];

/// Which of the four an object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Device,
    Adapter,
    Factory,
    Output,
}

impl Kind {
    fn iids(self) -> &'static [Iid] {
        match self {
            Kind::Device => &DEVICE_IIDS,
            Kind::Adapter => &ADAPTER_IIDS,
            Kind::Factory => &FACTORY_IIDS,
            Kind::Output => &OUTPUT_IIDS,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Device => "DXGI device",
            Kind::Adapter => "DXGI adapter",
            Kind::Factory => "DXGI factory",
            Kind::Output => "DXGI output",
        }
    }
}

/// One of the four. It lives as long as the process, so its count only counts.
#[repr(C)]
pub(crate) struct Part {
    object: Object,
    kind: Kind,
}

impl Part {
    pub(crate) fn add_ref(&self) -> u32 {
        self.object.add_ref()
    }
}

/// More positions than any of the four tables has.
const SLOTS: usize = 32;

const COMMON: [(usize, &str); 7] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "SetPrivateData"),
    (4, "SetPrivateDataInterface"),
    (5, "GetPrivateData"),
    (6, "GetParent"),
];

fn part(
    cell: &'static OnceLock<Part>,
    kind: Kind,
    interface: &str,
    names: &[(usize, &str)],
    methods: &[(usize, usize)],
) -> &'static Part {
    cell.get_or_init(|| {
        let mut all_names = COMMON.to_vec();
        all_names.extend_from_slice(names);
        let mut all_methods = vec![
            (0, query_interface as *const () as usize),
            (1, add_ref as *const () as usize),
            (2, release as *const () as usize),
            (6, parent as *const () as usize),
        ];
        all_methods.extend_from_slice(methods);
        Part {
            object: Object::new(object::table(interface, SLOTS, &all_names, &all_methods), 0),
            kind,
        }
    })
}

/// The DXGI device a graphics device answers as.
pub(crate) fn device() -> &'static Part {
    static PART: OnceLock<Part> = OnceLock::new();
    part(
        &PART,
        Kind::Device,
        "IDXGIDevice",
        &[
            (7, "GetAdapter"),
            (8, "CreateSurface"),
            (9, "QueryResourceResidency"),
            (10, "SetGPUThreadPriority"),
            (11, "GetGPUThreadPriority"),
            (12, "SetMaximumFrameLatency"),
            (13, "GetMaximumFrameLatency"),
        ],
        &[(7, get_adapter as *const () as usize)],
    )
}

fn adapter() -> &'static Part {
    static PART: OnceLock<Part> = OnceLock::new();
    part(
        &PART,
        Kind::Adapter,
        "IDXGIAdapter",
        &[
            (7, "EnumOutputs"),
            (8, "GetDesc"),
            (9, "CheckInterfaceSupport"),
            (10, "GetDesc1"),
            (11, "GetDesc2"),
        ],
        &[(7, first_output as *const () as usize)],
    )
}

fn factory() -> &'static Part {
    static PART: OnceLock<Part> = OnceLock::new();
    part(
        &PART,
        Kind::Factory,
        "IDXGIFactory",
        &[
            (7, "EnumAdapters"),
            (8, "MakeWindowAssociation"),
            (9, "GetWindowAssociation"),
            (10, "CreateSwapChain"),
            (11, "CreateSoftwareAdapter"),
            (12, "EnumAdapters1"),
            (13, "IsCurrent"),
        ],
        &[
            (7, first_adapter as *const () as usize),
            (12, first_adapter as *const () as usize),
        ],
    )
}

/// The one screen. A title passes it back when it sets its frame interval.
pub(crate) fn output() -> &'static Part {
    static PART: OnceLock<Part> = OnceLock::new();
    part(
        &PART,
        Kind::Output,
        "IDXGIOutput",
        &[
            (7, "GetDesc"),
            (8, "GetDisplayModeList"),
            (9, "FindClosestMatchingMode"),
            (10, "WaitForVBlank"),
            (11, "TakeOwnership"),
            (12, "ReleaseOwnership"),
            (13, "GetGammaControlCapabilities"),
            (14, "SetGammaControl"),
            (15, "GetGammaControl"),
            (16, "SetDisplaySurface"),
            (17, "GetDisplaySurfaceData"),
            (18, "GetFrameStatistics"),
        ],
        &[(21, modes as *const () as usize)],
    )
}

/// `DXGI_ERROR_MORE_DATA`: the caller's room is too small.
const MORE_DATA: Hresult = 0x887a_0003_u32 as i32;

/// A display mode as the console lists it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mode {
    width: u32,
    height: u32,
    /// Refreshes per second, as a numerator and a denominator.
    refresh: [u32; 2],
    format: u32,
    /// 1: progressive.
    scanlines: u32,
    scaling: u32,
    flags: u32,
}

const _: () = assert!(size_of::<Mode>() == 32);

/// The one mode the screen is said to have: the console's own, 3840 by 2160 at 60 Hz. What
/// the title draws is scaled to the window when it is presented.
pub(crate) const MODE: Mode = Mode {
    width: 3840,
    height: 2160,
    refresh: [60, 1],
    format: 0,
    scanlines: 1,
    scaling: 0,
    flags: 0,
};

/// The console's own mode list, the third method after the PC output's: `(format, flags,
/// count in and out, modes out)`. With no room given it says how many modes there are.
///
/// # Safety
/// `count` is null or the caller's count; `out` is null or room for that many modes.
unsafe extern "system" fn modes(
    _this: &Part,
    format: u32,
    flags: u32,
    count: *mut u32,
    out: *mut Mode,
) -> Hresult {
    // SAFETY: the caller's count; null is refused.
    let Some(count) = (unsafe { count.as_mut() }) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        *count = 1;
        return S_OK;
    }
    if *count < 1 {
        return MORE_DATA;
    }
    *count = 1;
    trace!("display modes asked for (format {format}, flags {flags:#x}): {MODE:?}");
    // SAFETY: checked non-null, and the caller says there is room for one.
    unsafe { out.write(MODE) };
    S_OK
}

/// Hands `part` out through `out` with a new reference, if the id at `iid` is `IUnknown`,
/// `IDXGIObject` or one of the part's own; else refuses, saying who asked whom for what.
///
/// # Safety
/// `iid` is null or points at an interface id; `out` is null or where the object goes.
unsafe fn hand(part: &'static Part, iid: *const Iid, out: *mut *const c_void) -> Hresult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the caller's id.
    let known = unsafe { object::is(iid, part.kind.iids()) || object::is(iid, &[OBJECT_IID]) };
    if !known {
        // SAFETY: the caller's id; null has no text.
        let id = unsafe { iid.as_ref() }.map(guid_text).unwrap_or_default();
        trace!(
            "the {} was asked for as interface {id}, which it is not",
            part.kind.name()
        );
        // SAFETY: checked non-null.
        unsafe { out.write(std::ptr::null()) };
        return E_NOINTERFACE;
    }
    part.object.add_ref();
    // SAFETY: checked non-null.
    unsafe { out.write(std::ptr::from_ref(part).cast()) };
    S_OK
}

/// `QueryInterface`. The DXGI device is also the graphics device.
///
/// # Safety
/// `iid` and `out` are null or valid.
unsafe extern "system" fn query_interface(
    this: &'static Part,
    iid: *const Iid,
    out: *mut *const c_void,
) -> Hresult {
    // SAFETY: the caller's id.
    if this.kind == Kind::Device && unsafe { crate::device::is_device(iid) } {
        // SAFETY: as received.
        return unsafe { crate::device::answer(iid, out.cast()) };
    }
    // SAFETY: as received.
    unsafe { hand(this, iid, out) }
}

extern "system" fn add_ref(this: &Part) -> u32 {
    this.object.add_ref()
}

extern "system" fn release(this: &Part) -> u32 {
    this.object.release()
}

/// `GetParent`: the device's and the output's parent is the adapter, the adapter's the
/// factory, and the factory has none.
///
/// # Safety
/// `iid` and `out` are null or valid.
unsafe extern "system" fn parent(this: &Part, iid: *const Iid, out: *mut *const c_void) -> Hresult {
    let parent = match this.kind {
        Kind::Device | Kind::Output => adapter(),
        Kind::Adapter => factory(),
        Kind::Factory => {
            // SAFETY: the caller's out-pointer; null is tolerated.
            if let Some(out) = unsafe { out.as_mut() } {
                *out = std::ptr::null();
            }
            return E_NOINTERFACE;
        }
    };
    // SAFETY: as received.
    unsafe { hand(parent, iid, out) }
}

/// `IDXGIDevice::GetAdapter`.
///
/// # Safety
/// `out` is null or where the adapter goes.
unsafe extern "system" fn get_adapter(_this: &Part, out: *mut *const c_void) -> Hresult {
    // SAFETY: a valid id and the caller's out-pointer.
    unsafe { hand(adapter(), &ADAPTER_IIDS[0], out) }
}

/// The one item of an enumeration at index 0, and `NOT_FOUND` past it.
///
/// # Safety
/// `out` is null or where the item goes.
unsafe fn only(item: &'static Part, index: u32, out: *mut *const c_void) -> Hresult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    if index != 0 {
        // SAFETY: checked non-null.
        unsafe { out.write(std::ptr::null()) };
        return NOT_FOUND;
    }
    // SAFETY: the item's own id and a checked out-pointer.
    unsafe { hand(item, &item.kind.iids()[0], out) }
}

/// `IDXGIAdapter::EnumOutputs`.
///
/// # Safety
/// `out` is null or where the output goes.
unsafe extern "system" fn first_output(
    _this: &Part,
    index: u32,
    out: *mut *const c_void,
) -> Hresult {
    // SAFETY: as received.
    unsafe { only(output(), index, out) }
}

/// `IDXGIFactory::EnumAdapters` and `EnumAdapters1`.
///
/// # Safety
/// `out` is null or where the adapter goes.
unsafe extern "system" fn first_adapter(
    _this: &Part,
    index: u32,
    out: *mut *const c_void,
) -> Hresult {
    // SAFETY: as received.
    unsafe { only(adapter(), index, out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::method;

    type Query =
        unsafe extern "system" fn(*const c_void, *const Iid, *mut *const c_void) -> Hresult;
    type Get = unsafe extern "system" fn(*const c_void, *mut *const c_void) -> Hresult;
    type Indexed = unsafe extern "system" fn(*const c_void, u32, *mut *const c_void) -> Hresult;

    #[test]
    fn a_title_walks_from_its_device_to_the_screen_and_back() {
        let d3d: *const c_void = std::ptr::from_ref(crate::device::device()).cast();
        // SAFETY: live objects whose tables have these methods at these positions.
        unsafe {
            let mut dxgi = std::ptr::null();
            let query: Query = method(d3d.cast(), 0);
            assert_eq!(query(d3d, &DEVICE_IIDS[0], &mut dxgi), S_OK);
            assert!(std::ptr::eq(dxgi.cast(), device()));

            let mut back = std::ptr::null();
            let query: Query = method(dxgi.cast(), 0);
            // Both are one object, so both give the same `IUnknown`.
            assert_eq!(query(dxgi, &object::IUNKNOWN, &mut back), S_OK);
            assert_eq!(back, d3d);
            let d3d_id = guid(
                0x189819f1,
                0x1db6,
                0x4b57,
                [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
            );
            assert_eq!(query(dxgi, &d3d_id, &mut back), S_OK);
            assert_eq!(back, d3d);

            let mut found = std::ptr::null();
            let get: Get = method(dxgi.cast(), 7);
            assert_eq!(get(dxgi, &mut found), S_OK);
            assert!(std::ptr::eq(found.cast(), adapter()));

            let mut screen = std::ptr::null();
            let outputs: Indexed = method(found.cast(), 7);
            assert_eq!(outputs(found, 0, &mut screen), S_OK);
            assert!(std::ptr::eq(screen.cast(), output()));
            assert_eq!(outputs(found, 1, &mut screen), NOT_FOUND);
            assert!(screen.is_null());

            let mut made_by = std::ptr::null();
            let parent: Query = method(found.cast(), 6);
            assert_eq!(parent(found, &FACTORY_IIDS[2], &mut made_by), S_OK);
            let adapters: Indexed = method(made_by.cast(), 12);
            let mut again = std::ptr::null();
            assert_eq!(adapters(made_by, 0, &mut again), S_OK);
            assert_eq!(again, found);
        }
    }

    #[test]
    fn the_screen_has_one_mode_and_says_so_before_it_is_given_room() {
        let mut count = 0;
        let mut mode = [Mode { width: 0, ..MODE }; 2];
        // SAFETY: a valid count, and room or null as each case needs.
        unsafe {
            assert_eq!(
                modes(output(), 0, 0, &mut count, std::ptr::null_mut()),
                S_OK
            );
            assert_eq!(count, 1);
            count = 2;
            assert_eq!(modes(output(), 0, 0, &mut count, mode.as_mut_ptr()), S_OK);
            assert_eq!((count, mode[0], mode[1].width), (1, MODE, 0));
            count = 0;
            assert_eq!(
                modes(output(), 0, 0, &mut count, mode.as_mut_ptr()),
                MORE_DATA
            );
            assert_eq!(
                modes(output(), 0, 0, std::ptr::null_mut(), mode.as_mut_ptr()),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn nothing_answers_as_another_of_the_four() {
        let mut out: *const c_void = std::ptr::dangling();
        // SAFETY: valid pointers, and the null one under test.
        unsafe {
            assert_eq!(hand(output(), &ADAPTER_IIDS[0], &mut out), E_NOINTERFACE);
            assert!(out.is_null());
            assert_eq!(hand(adapter(), &OBJECT_IID, &mut out), S_OK);
            assert_eq!(
                hand(adapter(), &ADAPTER_IIDS[0], std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!(parent(factory(), &FACTORY_IIDS[0], &mut out), E_NOINTERFACE);
            assert!(out.is_null());
        }
    }
}
