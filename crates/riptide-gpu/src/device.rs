//! The graphics device a title gets from `D3D12XboxCreateDevice`.
//!
//! There is one per process, as on the console. Its table starts like the PC device's
//! (`IUnknown`, the four object methods, then the device methods in PC order) and goes on
//! with the console's own. Methods that are not built name themselves and stop the title.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::object::{
    self, E_INVALIDARG, E_NOINTERFACE, E_POINTER, Hresult, Iid, Object, S_OK, guid, guid_text,
};
use crate::trace;

/// `ID3D12Device`, the same id as on PC.
const IID: Iid = guid(
    0x189819f1,
    0x1db6,
    0x4b57,
    [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
);

/// The newer levels of the device interface, `ID3D12Device1` to `ID3D12Device8` and
/// `ID3D12Device11`. Each level's table is the one before plus its methods, so one object
/// answers to all of them.
const LEVELS: [Iid; 9] = [
    guid(
        0x77acce80,
        0x638e,
        0x4e65,
        [0x88, 0x95, 0xc1, 0xf2, 0x33, 0x86, 0x86, 0x3e],
    ),
    guid(
        0x30baa41e,
        0xb15b,
        0x475c,
        [0xa0, 0xbb, 0x1a, 0xf5, 0xc5, 0xb6, 0x43, 0x28],
    ),
    guid(
        0x81dadc15,
        0x2bad,
        0x4392,
        [0x93, 0xc5, 0x10, 0x13, 0x45, 0xc4, 0xaa, 0x98],
    ),
    guid(
        0xe865df17,
        0xa9ee,
        0x46f9,
        [0xa4, 0x63, 0x30, 0x98, 0x31, 0x5a, 0xa2, 0xe5],
    ),
    guid(
        0x8b4f173b,
        0x2fea,
        0x4b80,
        [0x8f, 0x58, 0x43, 0x07, 0x19, 0x1a, 0xb9, 0x5d],
    ),
    guid(
        0xc70b221b,
        0x40e4,
        0x4a17,
        [0x89, 0xaf, 0x02, 0x5a, 0x07, 0x27, 0xa6, 0xdc],
    ),
    guid(
        0x5c014b53,
        0x68a1,
        0x4b9b,
        [0x8b, 0xd1, 0xdd, 0x60, 0x46, 0xb9, 0x35, 0x8b],
    ),
    guid(
        0x728f1fa2,
        0xea11,
        0x4a9f,
        [0x9b, 0x52, 0x9b, 0x42, 0xa4, 0x84, 0xd5, 0xc9],
    ),
    guid(
        0x5405c344,
        0xd457,
        0x444e,
        [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
    ),
];

/// Whether the id at `iid` names the device.
///
/// # Safety
/// `iid` is null or points at an interface id.
pub(crate) unsafe fn is_device(iid: *const Iid) -> bool {
    // SAFETY: the caller's pointer.
    unsafe { object::is(iid, &[IID]) || object::is(iid, &LEVELS) }
}

/// Set when the title creates its device as `ID3D12Device11`. Its queues and lists then
/// have two more methods each than older titles', although the interface version is the
/// same.
static NEWER: AtomicBool = AtomicBool::new(false);

pub(crate) fn newer() -> bool {
    NEWER.load(Ordering::Relaxed)
}

/// The version of the console's graphics interface these layouts are for (2.4): the first
/// word of the parameters a title creates its device with.
const INTERFACE_VERSION: u32 = 0x0002_0004;

/// More positions than the device's table has.
const SLOTS: usize = 160;

/// The device's methods by position, as far as their names are known. Positions 0 to 43
/// are the PC device's.
const NAMES: [(usize, &str); 62] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetNodeCount"),
    (8, "CreateCommandQueue"),
    (9, "CreateCommandAllocator"),
    (10, "CreateGraphicsPipelineState"),
    (11, "CreateComputePipelineState"),
    (12, "CreateCommandList"),
    (13, "CheckFeatureSupport"),
    (14, "CreateDescriptorHeap"),
    (15, "GetDescriptorHandleIncrementSize"),
    (16, "CreateRootSignature"),
    (17, "CreateConstantBufferView"),
    (18, "CreateShaderResourceView"),
    (19, "CreateUnorderedAccessView"),
    (20, "CreateRenderTargetView"),
    (21, "CreateDepthStencilView"),
    (22, "CreateSampler"),
    (23, "CopyDescriptors"),
    (24, "CopyDescriptorsSimple"),
    (25, "GetResourceAllocationInfo"),
    (26, "GetCustomHeapProperties"),
    (27, "CreateCommittedResource"),
    (28, "CreateHeap"),
    (29, "CreatePlacedResource"),
    (30, "CreateReservedResource"),
    (31, "CreateSharedHandle"),
    (32, "OpenSharedHandle"),
    (33, "OpenSharedHandleByName"),
    (34, "MakeResident"),
    (35, "Evict"),
    (36, "CreateFence"),
    (37, "GetDeviceRemovedReason"),
    (38, "GetCopyableFootprints"),
    (39, "CreateQueryHeap"),
    (40, "SetStablePowerState"),
    (41, "CreateCommandSignature"),
    (42, "GetResourceTiling"),
    (43, "GetAdapterLuid"),
    (48, "CreateCommandQueueX"),
    (61, "CreateSamplerX"),
    (62, "CreateComputePipelineStateX"),
    (69, "SetDebugErrorFilterX"),
    (70, "CreatePlacedShaderResourceViewX"),
    (71, "CreatePlacedUnorderedAccessViewX"),
    (73, "CreateCommandListX"),
    (74, "RegisterPagePoolX"),
    (75, "UnregisterPagePoolX"),
    (76, "CreateCommittedResourceX"),
    (78, "SetFrameIntervalX"),
    (79, "ScheduleFrameEventX"),
    (80, "WaitFrameEventX"),
    (81, "GetFrameStatisticsX"),
    (86, "CreatePipelineState"),
    (108, "GetResourceAllocationInfo2"),
    (127, "CreateCommittedResource3"),
    (133, "CreateResourceAtAddressX"),
];

/// The device as a title's code sees it: the object header, then two fields its own
/// compiled code reads.
#[repr(C)]
pub(crate) struct Device {
    object: Object,
    /// The graphics address of a constant buffer of nothing. None is made.
    null_constant_buffer: u64,
    /// Byte 0 not zero: calls the title's code would otherwise do itself, with the console's
    /// own structures, come to the table instead. The rest is the console's and stays zero.
    out_of_line: [u32; 16],
}

const _: () = assert!(size_of::<Device>() == 0x58);
const _: () = assert!(std::mem::offset_of!(Device, out_of_line) == 0x18);

/// The one device. It lives as long as the process, so its count only counts.
pub(crate) fn device() -> &'static Device {
    static DEVICE: OnceLock<Device> = OnceLock::new();
    DEVICE.get_or_init(|| {
        let methods = [
            (0, query_interface as *const () as usize),
            (1, add_ref as *const () as usize),
            (2, release as *const () as usize),
            (7, node_count as *const () as usize),
            (8, crate::queue::create as *const () as usize),
            (9, crate::list::create_allocator as *const () as usize),
            (12, crate::list::create_list as *const () as usize),
            (14, crate::descriptor::create as *const () as usize),
            (15, crate::descriptor::increment_size as *const () as usize),
            (16, crate::root::create as *const () as usize),
            (17, crate::view::create_constants as *const () as usize),
            (
                18,
                crate::view::create_shader_resource as *const () as usize,
            ),
            (
                19,
                crate::view::create_unordered_access as *const () as usize,
            ),
            (20, crate::view::create_render_target as *const () as usize),
            (21, crate::view::create_depth_target as *const () as usize),
            (22, crate::view::create_sampler as *const () as usize),
            (23, crate::view::copy as *const () as usize),
            (24, crate::view::copy_simple as *const () as usize),
            (36, crate::fence::create as *const () as usize),
            (39, crate::query::create as *const () as usize),
            (41, crate::signature::create as *const () as usize),
            (73, crate::list::create_list_x as *const () as usize),
            (48, crate::queue::create_x as *const () as usize),
            (61, crate::view::create_sampler_x as *const () as usize),
            (
                70,
                crate::view::create_placed_shader_resource as *const () as usize,
            ),
            (
                71,
                crate::view::create_placed_unordered_access as *const () as usize,
            ),
            (74, crate::pagepool::register as *const () as usize),
            (75, crate::pagepool::unregister as *const () as usize),
            (78, crate::frame::set_interval as *const () as usize),
            (86, crate::pipeline::create as *const () as usize),
            (
                133,
                crate::resource::create_at_address as *const () as usize,
            ),
            (79, crate::frame::schedule as *const () as usize),
            (80, crate::frame::wait as *const () as usize),
            (
                26,
                crate::resource::custom_heap_properties as *const () as usize,
            ),
            (25, crate::resource::allocation_info as *const () as usize),
            (27, crate::resource::create_committed as *const () as usize),
            (
                127,
                crate::resource::create_committed3 as *const () as usize,
            ),
            (108, crate::resource::allocation_info2 as *const () as usize),
        ];
        let mut out_of_line = [0u32; 16];
        out_of_line[0] = 1;
        Device {
            object: Object::new(object::table("ID3D12Device", SLOTS, &NAMES, &methods), 0),
            null_constant_buffer: 0,
            out_of_line,
        }
    })
}

/// Hands the device out as the interface at `iid`, with a new reference, or refuses.
///
/// # Safety
/// `iid` is null or points at an interface id; `out` is null or where the object goes.
pub(crate) unsafe fn answer(iid: *const Iid, out: *mut *const Device) -> Hresult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the caller's id.
    if !unsafe { is_device(iid) } {
        // The device is also a DXGI device. `IUnknown` was answered above, so only the
        // DXGI ids themselves get here.
        // SAFETY: the caller's id.
        if unsafe { iid.as_ref() }.is_some_and(|id| crate::dxgi::DEVICE_IIDS.contains(id)) {
            let dxgi = crate::dxgi::device();
            dxgi.add_ref();
            // SAFETY: checked non-null.
            unsafe { out.write(std::ptr::from_ref(dxgi).cast()) };
            return S_OK;
        }
        // SAFETY: the caller's id; null has no text.
        let id = unsafe { iid.as_ref() }.map(guid_text).unwrap_or_default();
        trace!("the graphics device was asked for as interface {id}, which it is not");
        // SAFETY: checked non-null.
        unsafe { out.write(std::ptr::null()) };
        return E_NOINTERFACE;
    }
    let device = device();
    device.object.add_ref();
    // SAFETY: checked non-null.
    unsafe { out.write(device) };
    S_OK
}

/// `d3d12_xs.dll!D3D12XboxCreateDevice`: `(adapter, parameters, interface id, device out)`.
///
/// The parameters are 0x40 bytes and start with the interface version. The rest sizes the
/// console's own buffers, which are not made here.
///
/// # Safety
/// The title's pointers, as the console's function takes them: each null or valid.
pub unsafe extern "system" fn create(
    _adapter: *const c_void,
    parameters: *const [u32; 16],
    iid: *const Iid,
    out: *mut *const c_void,
) -> Hresult {
    // SAFETY: the title's parameters; null is refused.
    let Some(parameters) = (unsafe { parameters.as_ref() }) else {
        return E_INVALIDARG;
    };
    trace!("D3D12XboxCreateDevice(parameters {parameters:x?})");
    if parameters[0] != INTERFACE_VERSION {
        trace!(
            "the title asks for graphics interface version {:#x}; the layouts here are for \
             {INTERFACE_VERSION:#x}",
            parameters[0]
        );
    }
    if let Err(code) = crate::host::host() {
        return code;
    }
    // SAFETY: the title's id; null is not the newest level.
    if unsafe { iid.as_ref() } == Some(&LEVELS[8]) {
        NEWER.store(true, Ordering::Relaxed);
        trace!("the device is created as ID3D12Device11: queues and lists have the newer layouts");
    }
    // SAFETY: the title's id and out-pointer.
    unsafe { answer(iid, out.cast()) }
}

/// `QueryInterface`.
///
/// # Safety
/// `iid` is null or points at an interface id; `out` is null or where the object goes.
unsafe extern "system" fn query_interface(
    _this: *const Device,
    iid: *const Iid,
    out: *mut *const Device,
) -> Hresult {
    // SAFETY: as received.
    unsafe { answer(iid, out) }
}

extern "system" fn add_ref(this: &Device) -> u32 {
    this.object.add_ref()
}

extern "system" fn release(this: &Device) -> u32 {
    this.object.release()
}

/// `GetNodeCount`: one graphics card.
extern "system" fn node_count(_this: &Device) -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOTIMPL, method};

    #[test]
    fn the_device_is_handed_out_at_every_level_and_as_nothing_else() {
        for iid in std::iter::once(&IID)
            .chain(&LEVELS)
            .chain([&object::IUNKNOWN])
        {
            let mut found = std::ptr::null();
            // SAFETY: a valid id and out-pointer.
            assert_eq!(unsafe { answer(iid, &mut found) }, S_OK);
            assert!(std::ptr::eq(found, device()));
        }
        let mut found: *const Device = device();
        // SAFETY: valid pointers, and null ones.
        unsafe {
            assert_eq!(answer(&[9u8; 16], &mut found), E_NOINTERFACE);
            assert!(found.is_null());
            assert_eq!(answer(&IID, std::ptr::null_mut()), E_POINTER);
        }
    }

    #[test]
    fn a_title_is_told_to_call_the_table_and_not_to_inline() {
        let device = std::ptr::from_ref(device()).cast::<u8>();
        // The title's code tests the byte at +0x18 before a device call.
        // SAFETY: the device is 0x58 bytes.
        assert_ne!(unsafe { device.add(0x18).read() }, 0);
    }

    #[test]
    fn built_methods_answer_and_the_others_do_not() {
        let device = std::ptr::from_ref(device()).cast::<Object>();
        // SAFETY: the table has `SLOTS` positions, and these have these shapes.
        unsafe {
            let count: extern "system" fn(*const Object) -> u32 = method(device, 7);
            assert_eq!(count(device), 1);
            let missing: extern "system" fn() -> Hresult = method(device, SLOTS - 1);
            assert_eq!(missing(), E_NOTIMPL);
        }
    }

    #[test]
    fn creation_refuses_missing_parameters() {
        let mut out = std::ptr::null();
        // SAFETY: null parameters are the case under test.
        let result = unsafe { create(std::ptr::null(), std::ptr::null(), &IID, &mut out) };
        assert_eq!(result, E_INVALIDARG);
    }
}
