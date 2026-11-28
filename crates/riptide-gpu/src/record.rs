//! What a title records on a command list.
//!
//! Each call becomes host calls on the host list at once (decision D6), where the host has
//! the same notion: viewports, scissors, topology, blend factor, stencil reference.
//!
//! What a draw needs beyond that (the pipeline, the root signature and its arguments, the
//! descriptor heaps, vertex and index buffers) only means something to a host pipeline, and
//! none can be made until shader programs are translated. Those calls are accepted and
//! nothing is kept of them, and draws and dispatches are counted and not made. That is the
//! whole of what is missing between this and a picture, and it is said in the session log
//! the first time a draw is skipped.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_VIEWPORT, D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT,
    D3D12_WRITEBUFFERIMMEDIATE_PARAMETER, ID3D12GraphicsCommandList2,
};
use windows::core::Interface;

use crate::list::List;
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
    if !rectangles.is_empty() {
        // SAFETY: a plain call on the host list.
        unsafe { this.host().RSSetScissorRects(rectangles) };
    }
}

/// `IASetPrimitiveTopology`.
extern "system" fn topology(this: &List, topology: u32) {
    // SAFETY: a plain call on the host list.
    unsafe {
        this.host()
            .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(topology as i32))
    };
}

/// `OMSetBlendFactor`: `(four factors, or null for all ones)`.
///
/// # Safety
/// `factor` is null or four numbers.
unsafe extern "system" fn blend_factor(this: &List, factor: *const [f32; 4]) {
    // SAFETY: the title's factors; a plain call on the host list.
    unsafe { this.host().OMSetBlendFactor(factor.as_ref()) };
}

/// `OMSetStencilRef`.
extern "system" fn stencil_reference(this: &List, reference: u32) {
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
/// Not built: the host GPU cannot wait inside a list, so the wait has to be made between
/// two host lists when they are submitted. Until submission is built the call is shown and
/// nothing waits.
extern "system" fn wait_on_value(this: &List, address: u64, comparison: u32, reference: u64) {
    static SHOWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SHOWN.fetch_add(1, Ordering::Relaxed) < 8 {
        trace!(
            "list {:p}: a GPU wait on the value at {address:#x} (comparison {comparison}, reference {reference:#x}) is not made",
            std::ptr::from_ref(this)
        );
    }
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

/// State that only a host pipeline can use: accepted, and nothing is kept. The methods
/// bound to this take different arguments, none of which is read.
extern "system" fn waits_for_pipelines(_this: &List) {}

/// Draws and dispatches not made so far, for want of host pipelines.
static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// A draw or a dispatch: counted and not made. The methods bound to this take different
/// arguments, none of which is read.
extern "system" fn skipped_draw(_this: &List) {
    let skipped = SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if skipped == 1 {
        trace!(
            "the title's first draw or dispatch is skipped, as every one will be: no host \
             pipeline exists, because shader programs cannot be translated yet"
        );
    } else if skipped.is_power_of_two() && skipped >= 1024 {
        trace!("{skipped} draws and dispatches skipped so far");
    }
}

/// The recording methods by their position in the older list layout.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    let state = waits_for_pipelines as *const () as usize;
    let draw = skipped_draw as *const () as usize;
    let mut methods = vec![
        (20, topology as *const () as usize),
        (21, viewports as *const () as usize),
        (22, scissors as *const () as usize),
        (23, blend_factor as *const () as usize),
        (24, stencil_reference as *const () as usize),
        (26, barrier as *const () as usize),
        (69, write_32 as *const () as usize),
        (70, write_64 as *const () as usize),
        (71, wait_on_value as *const () as usize),
        (72, wait_on_value as *const () as usize),
    ];
    // Draws and dispatches: instanced, indexed, dispatch, indirect, and the console's own
    // dispatch and mesh dispatches.
    methods.extend([12, 13, 14, 59, 78, 98, 127].map(|slot| (slot, draw)));
    // The pipeline, descriptor heaps, root signatures and root arguments, index and vertex
    // buffers.
    methods.extend((32..=48).chain([25]).map(|slot| (slot, state)));
    methods
}

/// The recording methods only lists of the newer layout have, by their position there.
pub(crate) fn newer_methods() -> Vec<(usize, usize)> {
    vec![(146, barrier as *const () as usize)]
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
