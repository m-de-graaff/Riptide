//! Command allocators and command lists.
//!
//! A title's own compiled code writes command packets straight into a console list while the
//! list's put pointer is below a limit, and calls the list's method otherwise. The lists
//! made here keep the pointer and every limit at zero, so every command arrives as a call.
//!
//! Each list records onto a host command list as its methods are called (decision D6).

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_FLAG_NONE, D3D12_COMMAND_LIST_TYPE, D3D12_RESOURCE_BARRIER,
    D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
    D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
    D3D12_RESOURCE_BARRIER_TYPE_UAV, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATES,
    D3D12_RESOURCE_TRANSITION_BARRIER, D3D12_RESOURCE_UAV_BARRIER, ID3D12CommandAllocator,
    ID3D12Device4, ID3D12GraphicsCommandList, ID3D12Resource,
};

use windows::core::Interface;

use crate::device::Device;
use crate::object::{self, Child, E_INVALIDARG, Hresult, Iid, Object, S_OK, guid};
use crate::trace;

/// `ID3D12CommandAllocator`.
const ALLOCATOR_IID: Iid = guid(
    0x6102dee4,
    0xaf59,
    0x4b09,
    [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
);

/// The list interfaces one list answers to: `ID3D12CommandList`,
/// `ID3D12GraphicsCommandList` and its levels 1 to 4, whose ids are PC's, then the console's
/// own levels 5 and 6.
const LIST_IIDS: [Iid; 8] = [
    guid(
        0x7116d91c,
        0xe7e4,
        0x47ce,
        [0xb8, 0xc6, 0xec, 0x81, 0x68, 0xf4, 0x37, 0xe5],
    ),
    guid(
        0x5b160d0f,
        0xac1b,
        0x4185,
        [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
    ),
    guid(
        0x553103fb,
        0x1fe7,
        0x4557,
        [0xbb, 0x38, 0x94, 0x6d, 0x7d, 0x0e, 0x7c, 0xa7],
    ),
    guid(
        0x38c3e585,
        0xff17,
        0x412c,
        [0x91, 0x50, 0x4f, 0xc6, 0xf9, 0xd7, 0x2a, 0x28],
    ),
    guid(
        0x6fda83a7,
        0xb84c,
        0x4e38,
        [0x9a, 0xc8, 0xc7, 0xbd, 0x22, 0x01, 0x6b, 0x3d],
    ),
    guid(
        0x8754318e,
        0xd3a9,
        0x4541,
        [0x98, 0xcf, 0x64, 0x5b, 0x50, 0xdc, 0x48, 0x74],
    ),
    guid(
        0x55050859,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    guid(
        0xc3827890,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
];

/// An allocator: the memory a list's commands are recorded into.
#[repr(C)]
pub(crate) struct Allocator {
    object: Object,
    kind: u32,
    host: ID3D12CommandAllocator,
}

// SAFETY: `Allocator` is `#[repr(C)]` and starts with its header.
unsafe impl Child for Allocator {
    const NAME: &'static str = "command allocator";
    const IIDS: &'static [Iid] = &[ALLOCATOR_IID];

    fn header(&self) -> &Object {
        &self.object
    }
}

/// A list as a title's code sees it, then what is kept here.
#[repr(C)]
pub(crate) struct List {
    object: Object,
    kind: u32,
    /// The put pointer, then four limits it is compared with. All zero: no packet ever fits,
    /// so every command is a call.
    put: [u64; 5],
    reserved: [u32; 64],
    /// What the title's code keeps while it builds packets. Not read here.
    graphics_root_headers: [u32; 128],
    compute_root_headers: [u32; 128],
    pipeline_shadow: [u32; 8],
    /// Newer titles' lists are larger and their code writes past the fields above. How far
    /// is not known; this keeps what follows clear of it.
    newer: [u32; 256],
    /// The host list this one records onto. It is swapped for another when a submission
    /// takes it away to run later; only the thread that records the list touches it.
    host: std::cell::UnsafeCell<ID3D12GraphicsCommandList>,
    /// How many commands have gone to the host list since the last reset.
    recorded: std::sync::atomic::AtomicU32,
    /// A wait the GPU is to make before it runs the host list being recorded now.
    gate: std::sync::Mutex<Option<Wait>>,
    /// The pieces the list was cut into at waits before the one being recorded now, in
    /// order, each with the wait the GPU makes before it.
    earlier: std::sync::Mutex<Vec<(Option<Wait>, ID3D12GraphicsCommandList)>>,
    /// The host allocator the list records into, for a piece started after a cut.
    allocator: std::cell::UnsafeCell<Option<ID3D12CommandAllocator>>,
    /// The host textures this list has used since it was last reset, each with the state
    /// the list last put it in.
    textures:
        std::sync::Mutex<std::collections::HashMap<usize, (ID3D12Resource, D3D12_RESOURCE_STATES)>>,
}

/// A wait on a value in memory: the GPU goes on when the value at `address` compares as
/// asked with `reference`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wait {
    pub(crate) address: u64,
    /// As the host numbers comparisons: 2 less, 3 equal, 4 less or equal, 5 greater, 6 not
    /// equal, 7 greater or equal; 1 never and 8 always.
    pub(crate) comparison: u32,
    pub(crate) reference: u64,
    /// Whether the value is 64 bits wide, or 32.
    pub(crate) wide: bool,
}

impl Wait {
    /// Whether `value` lets the GPU go on.
    pub(crate) fn passes(&self, value: u64) -> bool {
        match self.comparison {
            1 => false,
            2 => value < self.reference,
            3 => value == self.reference,
            4 => value <= self.reference,
            5 => value > self.reference,
            6 => value != self.reference,
            7 => value >= self.reference,
            _ => true,
        }
    }
}

/// Host lists that have run and can record again, by list type. A list whose host list
/// was taken away to run later gets one of these in its place.
static SPARES: std::sync::Mutex<Vec<(u32, ID3D12GraphicsCommandList)>> =
    std::sync::Mutex::new(Vec::new());

/// Gives a host list that has been submitted back, to record again.
pub(crate) fn recycle(kind: u32, list: ID3D12GraphicsCommandList) {
    let mut spares = SPARES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if spares.len() < 256 {
        spares.push((kind, list));
    }
}

/// A closed host list of type `kind`: a spare, or a new one.
fn closed_host_list(kind: u32) -> Option<ID3D12GraphicsCommandList> {
    {
        let mut spares = SPARES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(at) = spares.iter().position(|&(spare, _)| spare == kind) {
            return Some(spares.swap_remove(at).1);
        }
    }
    let host = crate::host::host().ok()?;
    let device: ID3D12Device4 = host.device.cast().ok()?;
    // SAFETY: a plain call on the host device. A list made this way is closed and has no
    // allocator until it is reset.
    unsafe {
        device
            .CreateCommandList1(
                0,
                D3D12_COMMAND_LIST_TYPE(kind as i32),
                D3D12_COMMAND_LIST_FLAG_NONE,
            )
            .ok()
    }
}

const _: () = assert!(std::mem::offset_of!(List, kind) == 0x10);
const _: () = assert!(std::mem::offset_of!(List, put) == 0x18);
const _: () = assert!(std::mem::offset_of!(List, pipeline_shadow) + 32 == 0x560);

// SAFETY: `List` is `#[repr(C)]` and starts with its header.
unsafe impl Child for List {
    const NAME: &'static str = "command list";
    const IIDS: &'static [Iid] = &LIST_IIDS;

    fn header(&self) -> &Object {
        &self.object
    }
}

/// The console's list description, for `CreateCommandListX`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DescriptionX {
    kind: u32,
    flags: u32,
    nodes: u32,
    barrier_batch: u32,
}

const ALLOCATOR_SLOTS: usize = 24;

const ALLOCATOR_NAMES: [(usize, &str); 10] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
    (8, "Reset"),
    (9, "SetAllocationCallbackX"),
];

fn allocator_table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut methods = object::child_methods::<Allocator>().to_vec();
        methods.push((8, allocator_reset as *const () as usize));
        methods.push((9, allocator_callback as *const () as usize));
        object::table(
            "ID3D12CommandAllocator",
            ALLOCATOR_SLOTS,
            &ALLOCATOR_NAMES,
            &methods,
        )
    })
}

/// More positions than the list's table has.
const LIST_SLOTS: usize = 160;

/// The list's methods by position in the older layout, as far as their names are known.
/// Positions 0 to 59 are the PC list's.
const LIST_NAMES: [(usize, &str); 77] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
    (8, "GetType"),
    (9, "Close"),
    (10, "Reset"),
    (11, "ClearState"),
    (12, "DrawInstanced"),
    (13, "DrawIndexedInstanced"),
    (14, "Dispatch"),
    (15, "CopyBufferRegion"),
    (16, "CopyTextureRegion"),
    (17, "CopyResource"),
    (18, "CopyTiles"),
    (19, "ResolveSubresource"),
    (20, "IASetPrimitiveTopology"),
    (21, "RSSetViewports"),
    (22, "RSSetScissorRects"),
    (23, "OMSetBlendFactor"),
    (24, "OMSetStencilRef"),
    (25, "SetPipelineState"),
    (26, "ResourceBarrier"),
    (27, "ExecuteBundle"),
    (28, "BeginQuery"),
    (29, "EndQuery"),
    (30, "ResolveQueryData"),
    (31, "SetPredication"),
    (32, "SetDescriptorHeaps"),
    (33, "SetComputeRootSignature"),
    (34, "SetGraphicsRootSignature"),
    (35, "SetComputeRootDescriptorTable"),
    (36, "SetGraphicsRootDescriptorTable"),
    (37, "SetComputeRoot32BitConstant"),
    (38, "SetGraphicsRoot32BitConstant"),
    (39, "SetComputeRoot32BitConstants"),
    (40, "SetGraphicsRoot32BitConstants"),
    (41, "SetComputeRootConstantBufferView"),
    (42, "SetGraphicsRootConstantBufferView"),
    (43, "SetComputeRootShaderResourceView"),
    (44, "SetGraphicsRootShaderResourceView"),
    (45, "SetComputeRootUnorderedAccessView"),
    (46, "SetGraphicsRootUnorderedAccessView"),
    (47, "IASetIndexBuffer"),
    (48, "IASetVertexBuffers"),
    (49, "SOSetTargets"),
    (50, "OMSetRenderTargets"),
    (51, "ClearDepthStencilView"),
    (52, "ClearRenderTargetView"),
    (53, "ClearUnorderedAccessViewUint"),
    (54, "ClearUnorderedAccessViewFloat"),
    (55, "DiscardResource"),
    (56, "SetMarker"),
    (57, "BeginEvent"),
    (58, "EndEvent"),
    (59, "ExecuteIndirect"),
    (60, "CopyMemoryX"),
    (68, "OMSetDepthBounds"),
    (69, "WriteBufferImmediate32X"),
    (70, "WriteBufferImmediate64X"),
    (71, "WaitOnValue32X"),
    (72, "WaitOnValue64X"),
    (73, "SetPixelShaderDepthOrderX"),
    (78, "DispatchX"),
    (83, "CopyTextureRegionX"),
    (84, "CopyResourceX"),
    (98, "DispatchMeshX"),
    (99, "SetDriverHintX"),
    (124, "RSSetShadingRate"),
    (127, "DispatchMesh"),
    (129, "PipelineSyncTopX"),
    (130, "PipelineSyncBottomX"),
    (137, "ResetX"),
];

/// Where a method of the older layout is in the layout of a device created as
/// `ID3D12Device11`: two methods were added before `ExecuteIndirect`, so everything from
/// there on is two later.
const fn placed(slot: usize, newer: bool) -> usize {
    if newer && slot >= 59 { slot + 2 } else { slot }
}

fn list_table(newer: bool) -> &'static [usize] {
    static TABLES: [OnceLock<&'static [usize]>; 2] = [OnceLock::new(), OnceLock::new()];
    TABLES[usize::from(newer)].get_or_init(|| {
        let mut methods = object::child_methods::<List>().to_vec();
        methods.extend([
            (8, list_type as *const () as usize),
            (9, close as *const () as usize),
            (10, reset as *const () as usize),
            (129, sync as *const () as usize),
            (130, sync as *const () as usize),
            (137, reset_x as *const () as usize),
        ]);
        methods.extend(crate::record::methods());
        let names: Vec<(usize, &str)> = LIST_NAMES
            .iter()
            .map(|&(slot, name)| (placed(slot, newer), name))
            .collect();
        let mut methods: Vec<(usize, usize)> = methods
            .iter()
            .map(|&(slot, method)| (placed(slot, newer), method))
            .collect();
        if newer {
            methods.extend(crate::record::newer_methods());
        }
        object::table("ID3D12GraphicsCommandList", LIST_SLOTS, &names, &methods)
    })
}

/// `ID3D12Device::CreateCommandAllocator`: `(list type, interface id, allocator out)`.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create_allocator(
    _device: *const Device,
    kind: u32,
    iid: *const Iid,
    out: *mut *const Allocator,
) -> Hresult {
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let host = match crate::host::host() {
        Ok(host) => host,
        Err(code) => return code,
    };
    // SAFETY: a plain call on the host device.
    let made: windows::core::Result<ID3D12CommandAllocator> = unsafe {
        host.device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE(kind as i32))
    };
    let host = match made {
        Ok(allocator) => allocator,
        Err(error) => {
            trace!("the host refused a command allocator of type {kind}: {error}");
            return error.code().0;
        }
    };
    let allocator = Box::new(Allocator {
        object: Object::new(allocator_table(), 1),
        kind,
        host,
    });
    trace!(
        "command allocator {:p} made: type {kind}",
        &raw const *allocator
    );
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(allocator, out) }
}

/// Makes a list recording into `allocator`. Lists start open, as on PC.
///
/// # Safety
/// `allocator` is null or a live allocator; `pipeline` is null or a live pipeline; `iid` and
/// `out` are null or valid.
unsafe fn make_list(
    kind: u32,
    allocator: *const Allocator,
    pipeline: usize,
    iid: *const Iid,
    out: *mut *const List,
) -> Hresult {
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    // SAFETY: the title's allocator; null is refused.
    let Some(allocator) = (unsafe { allocator.as_ref() }) else {
        return E_INVALIDARG;
    };
    if pipeline != 0 {
        trace!("a command list starts with pipeline {pipeline:#x}; pipelines are not built");
    }
    let host = match crate::host::host() {
        Ok(host) => host,
        Err(code) => return code,
    };
    // SAFETY: a plain call on the host device with a live host allocator.
    let made: windows::core::Result<ID3D12GraphicsCommandList> = unsafe {
        host.device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE(kind as i32),
            &allocator.host,
            None,
        )
    };
    let host = match made {
        Ok(list) => list,
        Err(error) => {
            trace!("the host refused a command list of type {kind}: {error}");
            return error.code().0;
        }
    };
    let list = Box::new(List {
        object: Object::new(list_table(crate::device::newer()), 1),
        kind,
        put: [0; 5],
        reserved: [0; 64],
        graphics_root_headers: [0; 128],
        compute_root_headers: [0; 128],
        pipeline_shadow: [0; 8],
        newer: [0; 256],
        host: std::cell::UnsafeCell::new(host),
        recorded: std::sync::atomic::AtomicU32::new(0),
        gate: std::sync::Mutex::new(None),
        earlier: std::sync::Mutex::new(Vec::new()),
        allocator: std::cell::UnsafeCell::new(Some(allocator.host.clone())),
        textures: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    trace!("command list {:p} made: type {kind}", &raw const *list);
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(list, out) }
}

/// `ID3D12Device::CreateCommandList`: `(node mask, list type, allocator, first pipeline,
/// interface id, list out)`.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create_list(
    _device: *const Device,
    _nodes: u32,
    kind: u32,
    allocator: *const Allocator,
    pipeline: usize,
    iid: *const Iid,
    out: *mut *const List,
) -> Hresult {
    // SAFETY: as received.
    unsafe { make_list(kind, allocator, pipeline, iid, out) }
}

/// `ID3D12Device::CreateCommandListX`: as [`create_list`], with the type, flags, node mask
/// and barrier batch size in a description.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create_list_x(
    _device: *const Device,
    description: *const DescriptionX,
    allocator: *const Allocator,
    pipeline: usize,
    iid: *const Iid,
    out: *mut *const List,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(&description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    trace!("CreateCommandListX({description:?})");
    // SAFETY: as received.
    unsafe { make_list(description.kind, allocator, pipeline, iid, out) }
}

/// Allocator `SetAllocationCallbackX`: the title gives a routine of its own that the
/// console's allocator asks for the memory command packets are written into. Lists here
/// write no packets, so the routine is never asked and nothing of it is kept. What the
/// method's arguments are, beyond the object, is not known.
extern "system" fn allocator_callback(_this: &Allocator) {}

/// Allocator `Reset`: the memory of every list recorded into it is free again.
extern "system" fn allocator_reset(this: &Allocator) -> Hresult {
    // SAFETY: a plain call on the host allocator.
    match unsafe { this.host.Reset() } {
        Ok(()) => S_OK,
        Err(error) => {
            trace!(
                "the host refused to reset an allocator of type {}: {error}",
                this.kind
            );
            error.code().0
        }
    }
}

impl List {
    /// Has the host GPU finish every write shaders made through unordered access before it
    /// goes on. The host orders everything else in a list by itself.
    pub(crate) fn wait_for_shader_writes(&self) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        // SAFETY: a plain call on the host list; the barrier names no resource, so nothing
        // in it needs dropping.
        unsafe { self.host().ResourceBarrier(&[barrier]) };
    }

    /// Moves a host texture to the state a command needs, if this list does not have it
    /// there already.
    ///
    /// Lists are recorded on many threads in an order that is not the order they run in,
    /// so no list can know what state another left a texture in. Each list therefore takes
    /// every texture as being in the common state when it starts and puts it back when it
    /// is closed.
    pub(crate) fn use_texture(&self, texture: &ID3D12Resource, wanted: D3D12_RESOURCE_STATES) {
        let key = windows::core::Interface::as_raw(texture) as usize;
        let mut textures = self
            .textures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = textures
            .entry(key)
            .or_insert_with(|| (texture.clone(), D3D12_RESOURCE_STATE_COMMON));
        if entry.1 != wanted {
            self.transition(texture, entry.1, wanted);
            entry.1 = wanted;
        }
    }

    /// Records a change of state of all of a host texture.
    fn transition(
        &self,
        texture: &ID3D12Resource,
        before: D3D12_RESOURCE_STATES,
        after: D3D12_RESOURCE_STATES,
    ) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // SAFETY: a copy of the pointer without a reference of its own. The
                    // barrier is used for one call, during which `texture` is alive, and is
                    // never dropped.
                    pResource: unsafe { std::mem::transmute_copy(texture) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        };
        // SAFETY: a plain call on the host list.
        unsafe { self.current().ResourceBarrier(&[barrier]) };
    }

    /// Puts every host texture this list used back in the common state, and forgets them.
    /// The list holds them until then, so that one the title releases meanwhile outlives
    /// the commands that name it.
    fn settle_textures(&self, forget: bool) {
        let mut textures = self
            .textures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (texture, state) in textures.values_mut() {
            if *state != D3D12_RESOURCE_STATE_COMMON {
                self.transition(texture, *state, D3D12_RESOURCE_STATE_COMMON);
                *state = D3D12_RESOURCE_STATE_COMMON;
            }
        }
        if forget {
            textures.clear();
        }
    }

    /// The host list this one records onto, without counting a command.
    fn current(&self) -> &ID3D12GraphicsCommandList {
        // SAFETY: a list is recorded, closed, reset and submitted by one thread at a time,
        // which is the console's rule for a title and the host's own; so nothing swaps the
        // host list while this reference is in use.
        unsafe { &*self.host.get() }
    }

    /// The host list this one records onto, for a command that is recorded now.
    pub(crate) fn host(&self) -> &ID3D12GraphicsCommandList {
        self.recorded
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.current()
    }

    /// The host list, for a submission that runs it now and leaves it this list's own.
    pub(crate) fn submitted(&self) -> &ID3D12GraphicsCommandList {
        self.current()
    }

    /// Records a wait: the GPU is to go on with what follows only when a value in memory
    /// allows.
    ///
    /// The host cannot wait inside a list. If nothing has been recorded yet, the wait is
    /// made before the whole list. Otherwise the list is cut in two here: what was recorded
    /// so far is closed and kept as a piece of its own, and recording goes on in another
    /// host list, before which the wait is made. Returns whether the wait was taken.
    ///
    /// A host list starts with no state, so state set before a cut (viewports, render
    /// targets and the like) is not there after it. Nothing carries it over yet.
    pub(crate) fn wait_here(&self, wait: Wait) -> bool {
        use std::sync::atomic::Ordering;
        let mut gate = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.recorded.load(Ordering::Relaxed) == 0 {
            *gate = Some(wait);
            return true;
        }
        // SAFETY: as in `current`: only the thread recording this list touches these.
        let Some(allocator) = (unsafe { &*self.allocator.get() }).clone() else {
            return false;
        };
        let Some(next) = closed_host_list(self.kind) else {
            return false;
        };
        // SAFETY: plain calls on host lists. The list recorded so far is closed before the
        // next one starts recording into the same allocator, as the host requires.
        unsafe {
            self.settle_textures(false);
            if self.current().Close().is_err() || next.Reset(&allocator, None).is_err() {
                return false;
            }
            let before = std::mem::replace(&mut *self.host.get(), next);
            self.earlier
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((gate.take(), before));
        }
        *gate = Some(wait);
        self.recorded.store(0, Ordering::Relaxed);
        true
    }

    /// Whether the GPU has to wait before any part of this list.
    pub(crate) fn has_waits(&self) -> bool {
        self.gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
            || !self
                .earlier
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
    }

    /// The list type, as the host numbers them.
    pub(crate) fn kind(&self) -> u32 {
        self.kind
    }

    /// Takes the list's host lists away, to be run later than the title's submission
    /// returns: every piece in order, each with the wait the GPU makes before it. A closed
    /// host list is put in their place. The title may reset and record this list again at
    /// once, which must not touch commands that have not run yet.
    pub(crate) fn take_pieces(&self) -> Vec<(Option<Wait>, ID3D12GraphicsCommandList)> {
        let mut pieces = std::mem::take(
            &mut *self
                .earlier
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        match closed_host_list(self.kind) {
            Some(replacement) => {
                // SAFETY: as in `current`: only the thread submitting this list uses it.
                let last = unsafe { std::mem::replace(&mut *self.host.get(), replacement) };
                pieces.push((gate, last));
            }
            None => {
                trace!(
                    "the last part of a command list is not submitted: the host gave no list in its place"
                )
            }
        }
        pieces
    }
}

/// `GetType`.
extern "system" fn list_type(this: &List) -> u32 {
    this.kind
}

/// `Close`: recording ends; the list can be submitted.
extern "system" fn close(this: &List) -> Hresult {
    this.settle_textures(false);
    // SAFETY: a plain call on the host list.
    match unsafe { this.current().Close() } {
        Ok(()) => S_OK,
        Err(error) => {
            trace!("the host refused to close a command list: {error}");
            error.code().0
        }
    }
}

/// `Reset`: `(allocator, first pipeline)`. Recording starts again, into `allocator`.
///
/// # Safety
/// `allocator` is null or a live allocator.
unsafe extern "system" fn reset(
    this: &List,
    allocator: *const Allocator,
    pipeline: usize,
) -> Hresult {
    // SAFETY: the title's allocator; null is refused.
    let Some(allocator) = (unsafe { allocator.as_ref() }) else {
        return E_INVALIDARG;
    };
    // The first pipeline means nothing until host pipelines exist.
    let _ = pipeline;
    // What the list recorded before is gone, or has run: the host textures it held for
    // those commands are let go.
    this.settle_textures(true);
    // SAFETY: a plain call on the host list with a live host allocator.
    this.recorded.store(0, std::sync::atomic::Ordering::Relaxed);
    *this
        .gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    // Pieces that were never submitted can record again.
    let unsubmitted = std::mem::take(
        &mut *this
            .earlier
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for (_, piece) in unsubmitted {
        recycle(this.kind, piece);
    }
    // SAFETY: as in `current`: only the thread resetting this list touches it.
    unsafe { *this.allocator.get() = Some(allocator.host.clone()) };
    // SAFETY: a plain call on the host list with a live host allocator.
    match unsafe { this.current().Reset(&allocator.host, None) } {
        Ok(()) => S_OK,
        Err(error) => {
            trace!("the host refused to reset a command list: {error}");
            error.code().0
        }
    }
}

/// `ResetX`: `(allocator, first pipeline, flags)`. As `Reset`; the flags tune the console's
/// command memory, which is not used here.
///
/// # Safety
/// `allocator` is null or a live allocator.
unsafe extern "system" fn reset_x(
    this: &List,
    allocator: *const Allocator,
    pipeline: usize,
    _flags: u32,
) -> Hresult {
    // SAFETY: as received.
    unsafe { reset(this, allocator, pipeline) }
}

/// `PipelineSyncTopX` and `PipelineSyncBottomX`: `(flags, control, address, size)`. On the
/// console these make the GPU finish earlier work, and flush its caches over a range of
/// memory, before it goes on. The host orders a list's commands itself except for what
/// shaders write through unordered access, so the sync becomes a wait for all of that.
extern "system" fn sync(this: &List, flags: u32, control: u32, address: u64, size: u64) {
    static SHOWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        trace!(
            "pipeline sync on list {:p}: flags {flags:#x}, control {control:#x}, address {address:#x}, size {size:#x}",
            &raw const *this
        );
    }
    this.wait_for_shader_writes();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER};

    #[test]
    fn a_wait_passes_when_the_value_compares_as_asked() {
        let wait = |comparison| Wait {
            address: 0,
            comparison,
            reference: 5,
            wide: true,
        };
        assert!(wait(7).passes(5) && wait(7).passes(9) && !wait(7).passes(4));
        assert!(wait(2).passes(4) && !wait(2).passes(5));
        assert!(wait(3).passes(5) && !wait(3).passes(6));
        assert!(wait(6).passes(6) && !wait(6).passes(5));
        assert!(wait(4).passes(5) && wait(5).passes(6) && !wait(5).passes(5));
        assert!(!wait(1).passes(5) && wait(8).passes(0));
    }

    #[test]
    fn newer_layouts_have_two_more_methods_before_execute_indirect() {
        assert_eq!(placed(52, true), 52);
        assert_eq!(placed(59, true), 61);
        assert_eq!(placed(130, true), 132);
        assert_eq!(placed(59, false), 59);
        assert!(
            LIST_NAMES
                .iter()
                .all(|&(slot, _)| placed(slot, true) < LIST_SLOTS)
        );
    }

    #[test]
    fn no_method_has_two_names() {
        for names in [&LIST_NAMES[..], &ALLOCATOR_NAMES[..]] {
            let mut slots: Vec<usize> = names.iter().map(|&(slot, _)| slot).collect();
            slots.sort_unstable();
            slots.dedup();
            assert_eq!(slots.len(), names.len());
        }
    }

    #[test]
    fn nothing_is_made_for_a_request_that_is_refused() {
        let mut allocator: *const Allocator = std::ptr::dangling();
        let mut list: *const List = std::ptr::dangling();
        // SAFETY: valid pointers, and the null ones under test.
        unsafe {
            assert_eq!(
                create_allocator(std::ptr::null(), 0, &[9u8; 16], &mut allocator),
                E_NOINTERFACE
            );
            assert!(allocator.is_null());
            assert_eq!(
                create_allocator(std::ptr::null(), 0, &ALLOCATOR_IID, std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!(
                make_list(0, std::ptr::null(), 0, &[9u8; 16], &mut list),
                E_NOINTERFACE
            );
            assert_eq!(
                make_list(0, std::ptr::null(), 0, &LIST_IIDS[7], &mut list),
                E_INVALIDARG
            );
            assert!(list.is_null());
            assert_eq!(
                create_list_x(
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    &LIST_IIDS[7],
                    &mut list
                ),
                E_INVALIDARG
            );
        }
    }
}
