//! The page table as the GPU reads it (decision M4, and T5 of the shader design).
//!
//! A translated shader reads title memory by address. Title memory lies in banks, each a
//! host buffer, so the shader first has to learn which bank an address is in and where.
//! This table says so: one 32-bit entry for each 64 KiB of title addresses, holding the
//! number of the frame mapped there plus one, or zero where nothing is mapped. A frame's
//! number is its bank times 32,768 plus its place in the bank.
//!
//! The table covers title addresses below 2^37, which is 8 MiB of entries. It lies in
//! memory the CPU writes straight into and the GPU reads at video-memory speed where the
//! machine has it (a GPU upload heap), and in ordinary upload memory otherwise, which the
//! GPU reads far more slowly. The CPU only ever writes it: what it wrote last is kept in
//! `riptide-mem`, which is asked for what changed.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_FEATURE_D3D12_OPTIONS16, D3D12_FEATURE_DATA_D3D12_OPTIONS16, D3D12_HEAP_FLAG_NONE,
    D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE, D3D12_HEAP_TYPE_GPU_UPLOAD, D3D12_HEAP_TYPE_UPLOAD,
    D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_FLAG_NONE,
    D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_STATES,
    D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};

use crate::trace;

/// The size of what one entry stands for.
const PAGE: u64 = 64 << 10;
/// Title addresses the table covers: all below this.
pub(crate) const LIMIT: u64 = 1 << 37;
/// How many entries the table has.
const ENTRIES: usize = (LIMIT / PAGE) as usize;

/// The entry for a mapped frame.
const fn entry(frame: u32) -> u32 {
    frame + 1
}

struct Table {
    buffer: ID3D12Resource,
    /// The table's entries, as the CPU writes them. Never read.
    entries: *mut u32,
}

// SAFETY: the buffer is a free-threaded host object, and its mapping is only written
// while the table's lock is held.
unsafe impl Send for Table {}

/// The table once it has been made; `None` inside if the host could not make it.
static TABLE: Mutex<Option<Option<Table>>> = Mutex::new(None);

fn make() -> Option<Table> {
    let host = crate::host::host().ok()?;
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
    // SAFETY: a plain query with room for its answer.
    let fast = unsafe {
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS16,
                (&raw mut options).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS16>() as u32,
            )
            .is_ok()
    } && options.GPUUploadHeapSupported.as_bool();
    let kinds: &[(D3D12_HEAP_TYPE, D3D12_RESOURCE_STATES, &str)] = if fast {
        &[
            (
                D3D12_HEAP_TYPE_GPU_UPLOAD,
                D3D12_RESOURCE_STATE_COMMON,
                "video memory the CPU writes",
            ),
            (
                D3D12_HEAP_TYPE_UPLOAD,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                "upload memory",
            ),
        ]
    } else {
        &[(
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            "upload memory",
        )]
    };
    let description = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: (ENTRIES * 4) as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };
    for &(kind, state, name) in kinds {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: kind,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain calls on the host device and the new buffer; the buffer stays
        // mapped for as long as it lives.
        let mapped = unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &description,
                    state,
                    None,
                    &mut made,
                )
                .ok()
                .and(made)
                .and_then(|buffer| {
                    let mut entries = std::ptr::null_mut();
                    buffer.Map(0, None, Some(&mut entries)).ok()?;
                    Some((buffer, entries.cast::<u32>()))
                })
        };
        if let Some((buffer, entries)) = mapped {
            // SAFETY: the mapping is the whole buffer, of that many entries. A new table
            // says nothing is mapped anywhere.
            unsafe { entries.write_bytes(0, ENTRIES) };
            trace!("the GPU's page table is made: {ENTRIES} entries in {name}");
            return Some(Table { buffer, entries });
        }
    }
    trace!("the GPU's page table could not be made");
    None
}

/// What the table should say for `pages` pages from page `first`, given what is mapped
/// there: each stretch as its first address, first frame and frame count.
fn entries_for(first: usize, pages: usize, mapped: &[(u64, u32, u32)]) -> Vec<u32> {
    let mut entries = vec![0u32; pages];
    for &(address, frame, count) in mapped {
        let start = (address / PAGE) as usize;
        for index in 0..count as usize {
            if let Some(slot) = (start + index)
                .checked_sub(first)
                .and_then(|at| entries.get_mut(at))
            {
                *slot = entry(frame + index as u32);
            }
        }
    }
    entries
}

/// Brings the table up to date with title memory: every range whose mapping has changed
/// since the last time is written again. Called before lists are submitted, so that what
/// they run with is what the title had mapped when it submitted them.
pub(crate) fn sync() {
    let Some(services) = crate::services() else {
        return;
    };
    let mut table = TABLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(table) = table.get_or_insert_with(make) else {
        return;
    };
    for range in services.memory.take_remapped() {
        if range.start >= LIMIT {
            show_beyond(range.start);
            continue;
        }
        let end = range.end.min(LIMIT);
        let first = (range.start / PAGE) as usize;
        let pages = (end.div_ceil(PAGE) as usize).saturating_sub(first);
        let mapped = services.memory.mapped_in(range.start..end);
        let entries = entries_for(first, pages, &mapped);
        // SAFETY: `first + pages` is at most the table's length, and the mapping is
        // written only here, under the lock.
        unsafe {
            std::ptr::copy_nonoverlapping(entries.as_ptr(), table.entries.add(first), pages);
        }
    }
}

/// Says once that title memory lies where the table does not reach.
fn show_beyond(address: u64) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if !SHOWN.swap(true, Ordering::Relaxed) {
        trace!(
            "title memory at {address:#x} lies beyond the GPU's page table, which ends at \
             {LIMIT:#x}: shaders cannot read it"
        );
    }
}

/// The table's buffer, for a shader to read.
#[allow(dead_code)] // the first user is the host root signature of translated shaders
pub(crate) fn buffer() -> Option<ID3D12Resource> {
    let mut table = TABLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    table
        .get_or_insert_with(make)
        .as_ref()
        .map(|table| table.buffer.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_a_frame_plus_one_and_zero_is_nothing() {
        // Pages 10 to 13: frames 7 and 8 at pages 11 and 12.
        let mapped = [(11 * PAGE, 7, 2)];
        assert_eq!(entries_for(10, 4, &mapped), [0, 8, 9, 0]);
        // A stretch that starts before the range or ends after it is cut to the range.
        let wide = [(8 * PAGE, 100, 10)];
        assert_eq!(entries_for(10, 3, &wide), [103, 104, 105]);
        assert_eq!(entries_for(10, 2, &[]), [0, 0]);
    }

    #[test]
    fn the_table_covers_128_gib_in_8_mib() {
        assert_eq!(ENTRIES * 4, 8 << 20);
        assert_eq!(ENTRIES as u64 * PAGE, LIMIT);
    }
}
