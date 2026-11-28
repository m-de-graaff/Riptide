//! What a title records on a command list.
//!
//! Each call becomes host calls on the host list at once (decision D6), where the host has
//! the same notion: viewports, scissors, topology, blend factor, stencil reference.
//!
//! What a draw needs beyond that (the pipeline, the root signature and its arguments, the
//! descriptor heaps, vertex and index buffers) and the draws themselves are in `draw.rs`.

use std::sync::atomic::Ordering;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList1;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_CLEAR_FLAGS, D3D12_RESOURCE_STATE_DEPTH_WRITE, D3D12_RESOURCE_STATE_RENDER_TARGET,
    D3D12_SHADING_RATE, D3D12_SHADING_RATE_COMBINER, D3D12_VIEWPORT,
    D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT, D3D12_WRITEBUFFERIMMEDIATE_PARAMETER,
    ID3D12GraphicsCommandList2, ID3D12GraphicsCommandList5,
};
use windows::core::Interface;

use crate::list::{List, Wait};
use crate::trace;

/// `count` items at `items`, or none.
///
/// # Safety
/// `items` is null or points at `count` items.
unsafe fn items<'a, T>(items: *const T, count: u32) -> &'a [T] {
    if items.is_null() || count == 0 {
        return &[];
    }
    // SAFETY: as this function's contract says.
    unsafe { std::slice::from_raw_parts(items, count as usize) }
}

/// `RSSetViewports`: `(count, viewports)`.
///
/// # Safety
/// `viewports` is null or `count` viewports.
unsafe extern "system" fn viewports(this: &List, count: u32, viewports: *const D3D12_VIEWPORT) {
    // SAFETY: the title's viewports.
    let viewports = unsafe { items(viewports, count) };
    this.drawing().state().carried.viewports = viewports.to_vec();
    if !viewports.is_empty() {
        // SAFETY: a plain call on the host list.
        unsafe { this.host().RSSetViewports(viewports) };
    }
}

/// `RSSetScissorRects`: `(count, rectangles)`.
///
/// # Safety
/// `rectangles` is null or `count` rectangles.
unsafe extern "system" fn scissors(this: &List, count: u32, rectangles: *const RECT) {
    // SAFETY: the title's rectangles.
    let rectangles = unsafe { items(rectangles, count) };
    this.drawing().state().carried.scissors = rectangles.to_vec();
    if !rectangles.is_empty() {
        // SAFETY: a plain call on the host list.
        unsafe { this.host().RSSetScissorRects(rectangles) };
    }
}

/// `IASetPrimitiveTopology`.
///
/// The console has topologies the host does not: 17 is a list of rectangles, three
/// vertices each, which a pipeline for them draws as triangles and completes in a geometry
/// shader. The others of the console's own (a line loop, quads) have not been met; they
/// are said once and drawn as triangles.
extern "system" fn topology(this: &List, topology: u32) {
    crate::draw::note_topology(this, topology);
    let host = match topology {
        17 => 4,
        14..=32 => {
            show_once(&TOPOLOGY, || {
                format!("topology {topology} is the console's own and is drawn as triangles")
            });
            4
        }
        other => other,
    };
    this.drawing().state().carried.topology = Some(host);
    // SAFETY: a plain call on the host list.
    unsafe {
        this.host()
            .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
    };
}

static TOPOLOGY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `OMSetBlendFactor`: `(four factors, or null for all ones)`.
///
/// # Safety
/// `factor` is null or four numbers.
unsafe extern "system" fn blend_factor(this: &List, factor: *const [f32; 4]) {
    // SAFETY: the title's factors; null is all ones.
    let factor = unsafe { factor.as_ref() }.copied().unwrap_or([1.0; 4]);
    this.drawing().state().carried.blend_factor = Some(factor);
    // SAFETY: a plain call on the host list.
    unsafe { this.host().OMSetBlendFactor(Some(&factor)) };
}

/// `OMSetStencilRef`.
extern "system" fn stencil_reference(this: &List, reference: u32) {
    this.drawing().state().carried.stencil = Some(reference);
    // SAFETY: a plain call on the host list.
    unsafe { this.host().OMSetStencilRef(reference) };
}

/// Has the GPU write `words` to title memory at `address` when it has done everything
/// recorded before this, as the console's writes at the bottom of its pipeline do. Title
/// memory is a host buffer, so this is the host's own immediate write into it.
fn write_when_reached(list: &List, address: u64, words: &[u32]) {
    let bytes = words.len() as u64 * 4;
    let Some(place) = crate::inplace::find(address, bytes) else {
        show_once(&UNREACHABLE, || {
            format!(
                "a GPU write of {bytes} bytes to {address:#x} is not made: that is not title memory in one stretch of one bank"
            )
        });
        return;
    };
    let host: ID3D12GraphicsCommandList2 = match list.host().cast() {
        Ok(host) => host,
        Err(error) => {
            show_once(&UNREACHABLE, || {
                format!("the host list cannot write immediate values: {error}")
            });
            return;
        }
    };
    let writes: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = words
        .iter()
        .enumerate()
        .map(|(index, &word)| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
            Dest: place.gpu_address + index as u64 * 4,
            Value: word,
        })
        .collect();
    let modes = vec![D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT; writes.len()];
    // SAFETY: a plain call on the host list; both lists have `writes.len()` entries.
    unsafe {
        host.WriteBufferImmediate(writes.len() as u32, writes.as_ptr(), Some(modes.as_ptr()))
    };
}

static UNREACHABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TIMESTAMP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Writes a line to the session log the first time only.
fn show_once(shown: &std::sync::atomic::AtomicBool, line: impl FnOnce() -> String) {
    if !shown.swap(true, Ordering::Relaxed) {
        trace!("{}", line());
    }
}

/// `WriteBufferImmediate32X`: `(address, value, flush, flags)`.
extern "system" fn write_32(this: &List, address: u64, value: u32, _flush: u32, _flags: u32) {
    write_when_reached(this, address, &[value]);
}

/// `WriteBufferImmediate64X`: `(address, value, flush, flags)`. Flag 1 asks for the GPU's
/// timestamp in place of the value and flag 2 for the system's.
///
/// A timestamp is taken here when the command is recorded, not when the GPU reaches it,
/// so it is early by however long the list waits and runs. Nothing has needed better yet.
extern "system" fn write_64(this: &List, address: u64, value: u64, _flush: u32, flags: u32) {
    let value = if flags & 3 != 0 {
        show_once(&TIMESTAMP, || {
            "a GPU timestamp write is given the time of recording, not of running".to_owned()
        });
        timestamp()
    } else {
        value
    };
    write_when_reached(this, address, &[value as u32, (value >> 32) as u32]);
}

/// Windows' performance counter now.
fn timestamp() -> u64 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
    }
    let mut count = 0i64;
    // SAFETY: a valid out-pointer; the call cannot fail on any supported Windows.
    unsafe { QueryPerformanceCounter(&mut count) };
    count as u64
}

/// `WaitOnValue32X` and `WaitOnValue64X`: `(address, comparison, reference)`. On the
/// console the GPU stops here until the value in memory at `address` compares as asked
/// with `reference`.
///
/// The host GPU cannot wait inside a list, so the list is cut in two at the wait and the
/// queue makes the wait between the two parts when it submits them, in order, on a thread
/// of its own (see `list.rs` and `ordered.rs`).
fn wait_on_value(this: &List, wait: Wait) {
    static SHOWN: [std::sync::atomic::AtomicU32; 2] =
        [const { std::sync::atomic::AtomicU32::new(0) }; 2];
    let taken = this.wait_here(wait);
    if SHOWN[usize::from(taken)].fetch_add(1, Ordering::Relaxed) < 2 {
        trace!(
            "list {:p}: a GPU wait on the value at {:#x} (comparison {}, reference {:#x}) {}",
            std::ptr::from_ref(this),
            wait.address,
            wait.comparison,
            wait.reference,
            if taken {
                "is made by the queue at that point of the list"
            } else {
                "is not made: the list could not be cut there"
            }
        );
    }
}

/// `WaitOnValue32X`.
extern "system" fn wait_on_value_32(this: &List, address: u64, comparison: u32, reference: u64) {
    wait_on_value(
        this,
        Wait {
            address,
            comparison,
            reference: reference & 0xffff_ffff,
            wide: false,
        },
    );
}

/// `WaitOnValue64X`.
extern "system" fn wait_on_value_64(this: &List, address: u64, comparison: u32, reference: u64) {
    wait_on_value(
        this,
        Wait {
            address,
            comparison,
            reference,
            wide: true,
        },
    );
}

/// `ResourceBarrier` `(count, barriers)` and the newer barrier groups `(group count,
/// groups)`: the title says a resource changes use, or that writes to it must be over.
///
/// The console's states and layouts are looser than the host's and are not followed here.
/// Each host use of a title's texture moves the host texture to the state that use needs,
/// so what is left of a barrier is its ordering: shader writes made before it are finished
/// before what follows reads them. The arguments are not read.
extern "system" fn barrier(this: &List) {
    this.wait_for_shader_writes();
}

/// `OMSetRenderTargets`: `(count, render target descriptors, whether they are one range,
/// depth target descriptor)`. With one range the first descriptor is given and the others
/// follow it in its heap.
///
/// # Safety
/// `targets` is null or as many descriptor handles as the call says (one, for a range);
/// `depth` is null or a descriptor handle; every descriptor names a live resource.
unsafe extern "system" fn set_render_targets(
    this: &List,
    count: u32,
    targets: *const u64,
    one_range: i32,
    depth: *const u64,
) {
    let step = u64::from(crate::descriptor::increment(2).unwrap_or(0));
    let handles: Vec<u64> = (0..count.min(8))
        .map(|index| {
            // SAFETY: the title's handles, as the contract says.
            unsafe {
                match (targets.is_null(), one_range != 0) {
                    (true, _) => 0,
                    (false, true) => targets.read_unaligned() + u64::from(index) * step,
                    (false, false) => targets.add(index as usize).read_unaligned(),
                }
            }
        })
        .collect();
    // SAFETY: the title's handle; null is no depth target.
    let depth = unsafe { depth.as_ref() }.copied();
    // SAFETY: the title's descriptors, which name live resources.
    unsafe { bind_targets(this, &handles, depth) };
    this.drawing().state().carried.targets = Some((handles, depth));
}

/// Binds the render targets at the descriptors `handles` and the depth target at `depth`
/// on the host list.
///
/// # Safety
/// Every descriptor names a live resource.
unsafe fn bind_targets(this: &List, handles: &[u64], depth: Option<u64>) {
    let mut views = Vec::new();
    // The host textures bound as targets, which a draw leaves as they are.
    let mut bound = Vec::new();
    for &handle in handles {
        // SAFETY: the title's descriptor.
        let Some(target) = (unsafe { crate::target::render_target(handle) }) else {
            show_once(&NO_TARGET, || {
                format!("a render target at descriptor {handle:#x} has no host texture")
            });
            // Targets are bound by place, so one that is missing ends the list of them.
            break;
        };
        this.use_texture(&target.texture, D3D12_RESOURCE_STATE_RENDER_TARGET);
        bound.push(target.texture.as_raw() as usize);
        views.push(target.view);
    }
    let depth_view = depth.and_then(|handle| {
        // SAFETY: the title's descriptor.
        let target = unsafe { crate::target::depth_target(handle) }?;
        this.use_texture(&target.texture, D3D12_RESOURCE_STATE_DEPTH_WRITE);
        bound.push(target.texture.as_raw() as usize);
        Some(target.view)
    });
    this.drawing().state().targets = bound;
    // SAFETY: a plain call on the host list with views made on the host.
    unsafe {
        this.host().OMSetRenderTargets(
            views.len() as u32,
            (!views.is_empty()).then_some(views.as_ptr()),
            false,
            depth_view.as_ref().map(std::ptr::from_ref),
        )
    };
}

/// What a list set that the host keeps in the host list, and so loses when the list goes
/// on in another host list after a GPU wait.
#[derive(Debug, Default)]
pub(crate) struct Carried {
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    /// The topology as the host was given it.
    topology: Option<u32>,
    blend_factor: Option<[f32; 4]>,
    stencil: Option<u32>,
    /// The render target descriptors and the depth target descriptor.
    targets: Option<(Vec<u64>, Option<u64>)>,
}

/// Gives the host list the list is now recording onto everything that was set before it
/// started: after a GPU wait cut the list in two, the part after the wait draws with what
/// the part before it set.
pub(crate) fn carry_over(this: &List) {
    let carried = std::mem::take(&mut this.drawing().state().carried);
    // SAFETY: plain calls on the host list with what the title set before; the target
    // descriptors named live resources then and the title has not been back since.
    unsafe {
        let host = this.host();
        if !carried.viewports.is_empty() {
            host.RSSetViewports(&carried.viewports);
        }
        if !carried.scissors.is_empty() {
            host.RSSetScissorRects(&carried.scissors);
        }
        if let Some(topology) = carried.topology {
            host.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(topology as i32));
        }
        if let Some(factor) = &carried.blend_factor {
            host.OMSetBlendFactor(Some(factor));
        }
        if let Some(reference) = carried.stencil {
            host.OMSetStencilRef(reference);
        }
        if let Some((handles, depth)) = &carried.targets {
            bind_targets(this, handles, *depth);
        }
    }
    this.drawing().state().carried = carried;
}

static NO_TARGET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `ClearRenderTargetView`: `(descriptor, colour, rectangle count, rectangles)`.
///
/// # Safety
/// `colour` is null or four numbers; `rectangles` is null or `count` rectangles; the
/// descriptor names a live resource.
unsafe extern "system" fn clear_render_target(
    this: &List,
    handle: u64,
    colour: *const [f32; 4],
    count: u32,
    rectangles: *const RECT,
) {
    // SAFETY: the title's descriptor.
    let Some(target) = (unsafe { crate::target::render_target(handle) }) else {
        show_once(&NO_TARGET, || {
            format!("a render target at descriptor {handle:#x} has no host texture")
        });
        return;
    };
    // SAFETY: the title's colour; without one the clear is to nothing.
    let colour = unsafe { colour.as_ref() }.copied().unwrap_or_default();
    // SAFETY: the title's rectangles.
    let rectangles = unsafe { items(rectangles, count) };
    this.use_texture(&target.texture, D3D12_RESOURCE_STATE_RENDER_TARGET);
    // SAFETY: a plain call on the host list.
    unsafe {
        this.host().ClearRenderTargetView(
            target.view,
            &colour,
            (!rectangles.is_empty()).then_some(rectangles),
        )
    };
}

/// `ClearDepthStencilView`: `(descriptor, what to clear, depth, stencil, rectangle count,
/// rectangles)`.
///
/// # Safety
/// `rectangles` is null or `count` rectangles; the descriptor names a live resource.
unsafe extern "system" fn clear_depth_target(
    this: &List,
    handle: u64,
    what: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rectangles: *const RECT,
) {
    // SAFETY: the title's descriptor.
    let Some(target) = (unsafe { crate::target::depth_target(handle) }) else {
        show_once(&NO_TARGET, || {
            format!("a depth target at descriptor {handle:#x} has no host texture")
        });
        return;
    };
    // SAFETY: the title's rectangles.
    let rectangles = unsafe { items(rectangles, count) };
    this.use_texture(&target.texture, D3D12_RESOURCE_STATE_DEPTH_WRITE);
    // SAFETY: a plain call on the host list.
    unsafe {
        this.host().ClearDepthStencilView(
            target.view,
            D3D12_CLEAR_FLAGS(what as i32),
            depth,
            stencil,
            (!rectangles.is_empty()).then_some(rectangles),
        )
    };
}

/// `RSSetShadingRate`: `(rate, two combiners or null)`. How coarsely pixels are shaded.
/// The host has the same call where its GPU can do it; where it cannot, shading stays
/// fine, which is only slower.
///
/// # Safety
/// `combiners` is null or two combiners.
unsafe extern "system" fn shading_rate(
    this: &List,
    rate: u32,
    combiners: *const [D3D12_SHADING_RATE_COMBINER; 2],
) {
    if let Ok(host) = this.host().cast::<ID3D12GraphicsCommandList5>() {
        // SAFETY: a plain call on the host list; the combiners are the title's.
        unsafe {
            host.RSSetShadingRate(
                D3D12_SHADING_RATE(rate as i32),
                (!combiners.is_null()).then_some(combiners.cast()),
            )
        };
    }
}

/// `OMSetDepthBounds`: `(least, greatest)`. Pixels whose stored depth is outside are not
/// drawn. The host has the same call where its GPU can do it.
extern "system" fn depth_bounds(this: &List, least: f32, greatest: f32) {
    if let Ok(host) = this.host().cast::<ID3D12GraphicsCommandList1>() {
        // SAFETY: a plain call on the host list.
        unsafe { host.OMSetDepthBounds(least, greatest) };
    }
}

/// Names for a profiler (`SetMarker`, `BeginEvent`, `EndEvent`) and hints to the console
/// GPU's driver (`SetPixelShaderDepthOrderX`, `SetDriverHintX`, and one newer method whose
/// purpose is not known and whose result the title does not use): none changes what is
/// drawn, and nothing is done. The methods bound to this take different arguments, none
/// of which is read.
extern "system" fn changes_nothing(_this: &List) {}

/// The recording methods by their position in the older list layout.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    let mut methods = vec![
        (20, topology as *const () as usize),
        (21, viewports as *const () as usize),
        (22, scissors as *const () as usize),
        (23, blend_factor as *const () as usize),
        (24, stencil_reference as *const () as usize),
        (26, barrier as *const () as usize),
        (50, set_render_targets as *const () as usize),
        (51, clear_depth_target as *const () as usize),
        (52, clear_render_target as *const () as usize),
        (69, write_32 as *const () as usize),
        (70, write_64 as *const () as usize),
        (124, shading_rate as *const () as usize),
        (68, depth_bounds as *const () as usize),
        (71, wait_on_value_32 as *const () as usize),
        (72, wait_on_value_64 as *const () as usize),
    ];
    methods.extend([56, 57, 58, 73, 99].map(|slot| (slot, changes_nothing as *const () as usize)));
    methods
}

/// The recording methods only lists of the newer layout have, by their position there.
pub(crate) fn newer_methods() -> Vec<(usize, usize)> {
    vec![
        (142, changes_nothing as *const () as usize),
        (146, barrier as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recording_method_has_a_position_of_its_own() {
        let methods = methods();
        let mut slots: Vec<usize> = methods.iter().map(|&(slot, _)| slot).collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), methods.len());
        // None of them takes the place of a method the list builds itself.
        for built in [0, 1, 2, 7, 8, 9, 10, 129, 130, 137] {
            assert!(!slots.contains(&built));
        }
    }

    #[test]
    fn a_missing_list_of_items_is_no_items() {
        let one = [7u32];
        // SAFETY: null, and one item.
        unsafe {
            assert!(items::<u32>(std::ptr::null(), 3).is_empty());
            assert!(items(one.as_ptr(), 0).is_empty());
            assert_eq!(items(one.as_ptr(), 1), [7]);
        }
    }
}
