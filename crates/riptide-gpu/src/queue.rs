//! Command queues: where a title submits its command lists.
//!
//! Each console queue is one host queue of the same list type.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE, D3D12_COMMAND_QUEUE_DESC, D3D12_COMMAND_QUEUE_FLAG_NONE,
    ID3D12CommandQueue,
};

use crate::device::Device;
use crate::fence::Fence;
use crate::list::List;
use crate::list::Piece;
use crate::object::{self, Child, E_INVALIDARG, Hresult, Iid, Object, S_OK, guid};
#[cfg(test)]
use crate::object::{E_NOINTERFACE, E_POINTER};
use crate::ordered::{Job, Ordered};
use crate::resource::Resource;
use crate::trace;

/// `ID3D12CommandQueue`.
const IID: Iid = guid(
    0x0ec870a6,
    0x5d7e,
    0x4c22,
    [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
);

/// `D3D12_COMMAND_QUEUE_DESC`, as on PC.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Description {
    /// The type of list the queue runs: 0 direct, 2 compute, 3 copy.
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The console's own queue description: which of the GPU's engines the queue runs on, in
/// place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DescriptionX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// A queue as a title's code sees it: the object header, then the list type, which its own
/// code reads.
#[repr(C)]
pub(crate) struct Queue {
    object: Object,
    kind: u32,
    description: Description,
    host: ID3D12CommandQueue,
    /// The order of what is submitted, for when a submission has to wait.
    ordered: std::sync::Arc<Ordered>,
}

const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

// SAFETY: `Queue` is `#[repr(C)]` and starts with its header.
unsafe impl Child for Queue {
    const NAME: &'static str = "command queue";
    const IIDS: &'static [Iid] = &[IID];

    fn header(&self) -> &Object {
        &self.object
    }
}

/// More positions than the queue's table has.
const SLOTS: usize = 40;

/// The queue's methods by position in the older layout, as far as their names are known.
/// Positions 0 to 17 are the PC queue's.
const NAMES: [(usize, &str); 23] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
    (8, "UpdateTileMappings"),
    (9, "CopyTileMappings"),
    (10, "ExecuteCommandLists"),
    (11, "SetMarker"),
    (12, "BeginEvent"),
    (13, "EndEvent"),
    (14, "Signal"),
    (15, "Wait"),
    (16, "GetTimestampFrequency"),
    (17, "GetClockCalibration"),
    (26, "GetDesc"),
    (29, "CopyPageMappingsBatchX"),
    (30, "PresentX"),
    (32, "Signal2X"),
    (33, "ExecuteCommandListsX"),
];

/// Where a method of the older layout is in the layout of a device created as
/// `ID3D12Device11`: two methods were added before `Signal`, so everything from there on
/// is two later.
const fn placed(slot: usize, newer: bool) -> usize {
    if newer && slot >= 14 { slot + 2 } else { slot }
}

/// The queue's table for one of the two layouts, made the first time it is needed.
fn table(newer: bool) -> &'static [usize] {
    static TABLES: [std::sync::OnceLock<&'static [usize]>; 2] =
        [std::sync::OnceLock::new(), std::sync::OnceLock::new()];
    TABLES[usize::from(newer)].get_or_init(|| {
        let mut methods = object::child_methods::<Queue>().to_vec();
        methods.extend([
            (14, signal as *const () as usize),
            (15, wait as *const () as usize),
            (10, execute as *const () as usize),
            (11, marker as *const () as usize),
            (12, marker as *const () as usize),
            (13, marker as *const () as usize),
            (16, timestamp_frequency as *const () as usize),
            (29, copy_page_mappings as *const () as usize),
            (30, present as *const () as usize),
            (32, signal_2 as *const () as usize),
            (33, execute_x as *const () as usize),
            (17, clock_calibration as *const () as usize),
        ]);
        let names: Vec<(usize, &str)> = NAMES
            .iter()
            .map(|&(slot, name)| (placed(slot, newer), name))
            .collect();
        let methods: Vec<(usize, usize)> = methods
            .iter()
            .map(|&(slot, method)| (placed(slot, newer), method))
            .collect();
        object::table("ID3D12CommandQueue", SLOTS, &names, &methods)
    })
}

/// Makes a queue and hands it to the title as the interface at `iid`.
///
/// # Safety
/// `iid` is null or points at an interface id; `out` is null or where the object goes.
unsafe fn make(description: Description, iid: *const Iid, out: *mut *const Queue) -> Hresult {
    // SAFETY: the caller's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    let host = match crate::host::host() {
        Ok(host) => host,
        Err(code) => return code,
    };
    // The console's queue flags choose among its GPU's engines and mean nothing to the
    // host, which refuses the ones it does not know.
    let host_description = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE(description.kind as i32),
        Priority: 0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };
    // SAFETY: a plain call on the host device with a valid description.
    let made: windows::core::Result<ID3D12CommandQueue> =
        unsafe { host.device.CreateCommandQueue(&host_description) };
    let host = match made {
        Ok(queue) => queue,
        Err(error) => {
            trace!("the host refused a command queue ({description:?}): {error}");
            return error.code().0;
        }
    };
    let queue = Box::new(Queue {
        object: Object::new(table(crate::device::newer()), 1),
        kind: description.kind,
        description,
        ordered: Ordered::new(host.clone()),
        host,
    });
    trace!(
        "command queue {:p} made: {description:?}",
        &raw const *queue
    );
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(queue, out) }
}

/// `ID3D12Device::CreateCommandQueue`: `(description, interface id, queue out)`.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create(
    _device: *const Device,
    description: *const Description,
    iid: *const Iid,
    out: *mut *const Queue,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(&description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    // SAFETY: the title's id and out-pointer.
    unsafe { make(description, iid, out) }
}

/// `ID3D12Device::CreateCommandQueueX`: as [`create`], with the console's description. The
/// host chooses its own engines.
///
/// # Safety
/// The title's pointers: each null or valid.
pub(crate) unsafe extern "system" fn create_x(
    _device: *const Device,
    description: *const DescriptionX,
    iid: *const Iid,
    out: *mut *const Queue,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(&console) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    trace!("CreateCommandQueueX({console:?})");
    let description = Description {
        kind: console.kind,
        priority: 0,
        flags: console.flags,
        nodes: 1,
    };
    // SAFETY: the title's id and out-pointer.
    unsafe { make(description, iid, out) }
}

/// `Signal`: the fence gets `value` when the GPU has done everything submitted to this
/// queue so far.
///
/// # Safety
/// `fence` is null or a live fence.
unsafe extern "system" fn signal(this: &Queue, fence: *const Fence, value: u64) -> Hresult {
    // SAFETY: the title's fence; null is refused.
    let Some(fence) = (unsafe { fence.as_ref() }) else {
        return E_INVALIDARG;
    };
    // The fence's own value follows when the host GPU gets there, whenever that is.
    fence.expect(value);
    // In its turn, behind whatever was submitted before and has not run yet.
    this.ordered
        .submit(vec![Job::Signal(fence.host().clone(), value)], true);
    S_OK
}

/// `Wait`: the queue runs nothing more until the fence reaches `value`.
///
/// # Safety
/// `fence` is null or a live fence.
unsafe extern "system" fn wait(this: &Queue, fence: *const Fence, value: u64) -> Hresult {
    // SAFETY: the title's fence; null is refused.
    let Some(fence) = (unsafe { fence.as_ref() }) else {
        return E_INVALIDARG;
    };
    // SAFETY: a plain call on the host queue with a live host fence.
    match unsafe { this.host.Wait(fence.host(), value) } {
        Ok(()) => S_OK,
        Err(error) => error.code().0,
    }
}

/// `ExecuteCommandLists`: `(count, lists)`. The host lists the title's lists recorded onto
/// are submitted to the host queue at once, in the same order.
///
/// # Safety
/// `lists` is null or `count` pointers, each null or a live, closed list.
unsafe extern "system" fn execute(this: &Queue, count: u32, lists: *const *const List) {
    if lists.is_null() || count == 0 {
        return;
    }
    // SAFETY: the title passes `count` pointers.
    let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
    let lists: Vec<&List> = lists
        .iter()
        // SAFETY: each is null or a live list.
        .filter_map(|&list| unsafe { list.as_ref() })
        .collect();
    if lists.is_empty() {
        return;
    }
    for list in &lists {
        let before = list.count_submission();
        if before > 0 && RESUBMITTED.fetch_add(1, Ordering::Relaxed) < 6 {
            trace!(
                "list {:p} (type {}) is submitted again without a reset: {} times before",
                std::ptr::from_ref(*list),
                list.kind(),
                before
            );
        }
    }
    let submitted = SUBMITTED.fetch_add(lists.len() as u64, Ordering::Relaxed);
    if submitted == 0 {
        trace!("the title's first command lists are submitted to the host GPU");
    }
    // The lists' parts in order, each wait before the part it holds back.
    let mut waits = false;
    let mut jobs = Vec::new();
    let mut run: Vec<Piece> = Vec::new();
    for list in lists {
        for piece in list.pieces() {
            if let Some(wait) = piece.wait {
                waits = true;
                if !run.is_empty() {
                    jobs.push(Job::Lists(std::mem::take(&mut run)));
                }
                jobs.push(Job::Gate(wait));
            }
            run.push(piece);
        }
    }
    if !run.is_empty() {
        jobs.push(Job::Lists(run));
    }
    // With no wait, and nothing earlier still waiting, this runs now on the title's
    // thread. Otherwise it takes its turn on the queue's own.
    if !this.ordered.submit(jobs, !waits) && DEFERRED.fetch_add(1, Ordering::Relaxed) == 0 {
        trace!("a submission has to wait: its queue now submits in order on a thread of its own");
    }
}

/// Submissions that had to wait their turn.
static DEFERRED: AtomicU64 = AtomicU64::new(0);

/// Submissions of a list that had not been reset since its last one.
static RESUBMITTED: AtomicU64 = AtomicU64::new(0);

/// Lists submitted so far.
static SUBMITTED: AtomicU64 = AtomicU64::new(0);

/// `ExecuteCommandListsX`: `(count, lists, frame token, flags)`. As `ExecuteCommandLists`;
/// the token says which frame the work is for, which the host does not ask.
///
/// # Safety
/// As [`execute`].
unsafe extern "system" fn execute_x(
    this: &Queue,
    count: u32,
    lists: *const *const List,
    _token: u64,
    _flags: u32,
) {
    // SAFETY: as received.
    unsafe { execute(this, count, lists) };
}

/// `SetMarker`, `BeginEvent` and `EndEvent`: names for a profiler. Nothing to do.
extern "system" fn marker(_this: &Queue) {}

/// `Signal2X`: `(fence, value, two sets of console sync settings)`. As `Signal`: the host
/// queue signals when it has done everything submitted so far.
///
/// # Safety
/// `fence` is null or a live fence.
unsafe extern "system" fn signal_2(
    this: &Queue,
    fence: *const Fence,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> Hresult {
    // SAFETY: as received.
    unsafe { signal(this, fence, value) }
}

/// A run of destination pages and how they are mapped: kind 0 takes that many pool pages
/// from `start` on, kind 1 the one pool page `start` for each, kind 2 unmaps them, and
/// kind 3 leaves them as they are.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on, one after another.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// A page of a pool, and of what is mapped from it.
const PAGE: u64 = 64 << 10;

/// `CopyPageMappingsBatchX`: `(batch count, batches, pool address, pool pages, flags)`.
/// Pages of a pool (memory the title filled) are mapped at other addresses, which is how a
/// title gives streamed pages to its textures. On the console the GPU's page table
/// changes. Here the title's addresses are the GPU's, so the same frames of title memory
/// are mapped at the destination too, at once.
///
/// # Safety
/// `batches` is null or `count` batches, each with as many ranges as it says.
unsafe extern "system" fn copy_page_mappings(
    _this: &Queue,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> Hresult {
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let Some(services) = crate::services() else {
        return E_INVALIDARG;
    };
    let memory = services.memory;
    let batches = if count == 0 {
        &[][..]
    } else {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    };
    let mut failures = 0u32;
    let mut pages = 0u64;
    for batch in batches {
        if batch.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(batch.ranges, batch.count as usize) };
        let mut at = batch.destination;
        for range in ranges {
            let bytes = u64::from(range.count) * PAGE;
            let inside = |pages: u32| {
                range
                    .start
                    .checked_add(pages)
                    .is_some_and(|end| end <= pool_pages)
            };
            let source = pool + u64::from(range.start) * PAGE;
            let done = match range.kind {
                0 if inside(range.count) => memory.alias(at, source, bytes).is_ok(),
                1 if inside(1) => (0..u64::from(range.count))
                    .all(|page| memory.alias(at + page * PAGE, source, PAGE).is_ok()),
                2 => memory.unmap(at, bytes).is_ok(),
                3 => true,
                _ => false,
            };
            if !done {
                failures += 1;
                if MAPPING_FAILURES.fetch_add(1, Ordering::Relaxed) < 4 {
                    trace!(
                        "a page mapping failed: {range:?} at {at:#x} from the pool at {pool:#x} \
                         of {pool_pages} pages"
                    );
                }
            }
            pages += u64::from(range.count);
            at += bytes;
        }
    }
    if MAPPINGS.fetch_add(1, Ordering::Relaxed) < 4 {
        trace!("page mappings copied: {count} batches, {pages} pages, {failures} ranges failed");
    }
    S_OK
}

static MAPPINGS: AtomicU64 = AtomicU64::new(0);
static MAPPING_FAILURES: AtomicU64 = AtomicU64::new(0);

/// One plane of a present: the frame's token, then the textures of the plane. The console
/// goes on with rectangles, a colour space and options, which are not read here.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const *const Resource,
}

/// `PresentX`: `(plane count, planes, parameters)`. The first texture of the first plane
/// is shown in the window.
///
/// # Safety
/// `planes` is null or at least one plane, whose textures are live resources.
unsafe extern "system" fn present(
    this: &Queue,
    count: u32,
    planes: *const Plane,
    _parameters: *const u8,
) -> Hresult {
    let frames = PRESENTED.fetch_add(1, Ordering::Relaxed) + 1;
    // SAFETY: the title's planes; null or none is refused.
    let Some(plane) = (unsafe { planes.as_ref() }).filter(|_| count > 0) else {
        return E_INVALIDARG;
    };
    if plane.count == 0 || plane.resources.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the plane has at least one texture pointer, null or a live resource.
    let Some(resource) = (unsafe { plane.resources.read_unaligned().as_ref() }) else {
        return E_INVALIDARG;
    };
    // In its turn, behind whatever was submitted before and has not run yet.
    let shown = resource.host().is_some_and(|frame| {
        this.ordered.submit(vec![Job::Present(frame.clone())], true);
        true
    });
    if frames <= 3 || frames.is_power_of_two() {
        trace!(
            "frame {frames} presented ({count} planes, token {}): {}",
            plane.token,
            if shown {
                "given to the queue to show"
            } else {
                "not shown: it has no host texture"
            }
        );
    }
    S_OK
}

/// Frames presented so far.
static PRESENTED: AtomicU64 = AtomicU64::new(0);

/// `GetTimestampFrequency`: `(frequency out)`. How many ticks of the GPU's timer make a
/// second. Timestamps a title's lists take come from the host GPU, so the answer is the
/// host queue's.
///
/// # Safety
/// `out` is null or where the frequency goes.
unsafe extern "system" fn timestamp_frequency(this: &Queue, out: *mut u64) -> Hresult {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    // SAFETY: a plain call on the host queue.
    match unsafe { this.host.GetTimestampFrequency() } {
        Ok(frequency) => {
            *out = frequency;
            S_OK
        }
        Err(error) => {
            trace!("the host queue has no timestamp frequency: {error}");
            error.code().0
        }
    }
}

/// `GetClockCalibration`: `(GPU timestamp out, CPU timestamp out)`. The two clocks read at
/// the same moment, from the host queue.
///
/// # Safety
/// Each pointer is null or where its timestamp goes.
unsafe extern "system" fn clock_calibration(this: &Queue, gpu: *mut u64, cpu: *mut u64) -> Hresult {
    if gpu.is_null() || cpu.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a plain call on the host queue with checked out-pointers.
    match unsafe { this.host.GetClockCalibration(gpu, cpu) } {
        Ok(()) => S_OK,
        Err(error) => error.code().0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_layouts_have_two_more_methods_before_signal() {
        assert_eq!(placed(10, true), 10);
        assert_eq!(placed(14, true), 16);
        assert_eq!(placed(33, true), 35);
        assert_eq!(placed(14, false), 14);
        assert!(placed(33, true) < SLOTS);
    }

    #[test]
    fn a_queue_is_not_made_as_another_interface_or_without_a_description() {
        let description = Description {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut out: *const Queue = std::ptr::dangling();
        // SAFETY: valid pointers, and the null ones under test.
        unsafe {
            assert_eq!(make(description, &[9u8; 16], &mut out), E_NOINTERFACE);
            assert!(out.is_null());
            assert_eq!(make(description, &IID, std::ptr::null_mut()), E_POINTER);
            assert_eq!(
                create(std::ptr::null(), std::ptr::null(), &IID, &mut out),
                E_INVALIDARG
            );
        }
    }
}
