//! Copies a title records on a list.
//!
//! A console buffer is title memory, and the host GPU reads and writes title memory where
//! it lies, through one buffer over each bank (`inplace.rs`). So a copy from one console
//! buffer to another is a copy between two ranges of those bank buffers, made by the host
//! GPU at that point of the list: what the list wrote before it is copied, and what comes
//! after reads the copy.
//!
//! The host cannot copy within one buffer, and two ranges of title memory are often in
//! the same bank. Such a copy goes by way of a scratch buffer of the list's allocator:
//! out of the bank, and back in.
//!
//! Built: `CopyBufferRegion`, and the console's `CopyMemoryX`, which names two addresses
//! and no resource. Not built: copies of textures, which name themselves when
//! called, and a range that is not one stretch of one bank, which is said in the session
//! log and not copied.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_STATE_COPY_DEST,
    D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use crate::list::{List, READ};
use crate::resource::Resource;
use crate::trace;

/// A scratch buffer and its size. It rests as a copy's destination.
type Scratch = (ID3D12Resource, u64);

/// The scratch buffers each allocator has, by a number that stands for the allocator.
static SCRATCH: Mutex<Option<HashMap<usize, Vec<Scratch>>>> = Mutex::new(None);

/// The least a scratch buffer is made with.
const LEAST_SCRATCH: u64 = 1 << 20;

/// How large a scratch buffer is made for a copy of `bytes`: the next power of two, and
/// no less than [`LEAST_SCRATCH`], so that a run of growing copies makes few buffers.
fn scratch_size(bytes: u64) -> u64 {
    bytes.max(LEAST_SCRATCH).next_power_of_two()
}

/// A scratch buffer of at least `bytes` for copies recorded into `allocator`. The
/// commands of one allocator run one after another, so one buffer serves them all; it is
/// kept for the allocator's later lists.
fn scratch(allocator: usize, bytes: u64) -> Option<ID3D12Resource> {
    let mut all = SCRATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let held = all
        .get_or_insert_with(HashMap::new)
        .entry(allocator)
        .or_default();
    if let Some((buffer, _)) = held.iter().find(|(_, size)| *size >= bytes) {
        return Some(buffer.clone());
    }
    let host = crate::host::host().ok()?;
    let size = scratch_size(bytes);
    let description = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: a plain call with descriptions that outlive it.
    let made = unsafe {
        host.device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &description,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut buffer,
        )
    };
    if let Err(error) = made {
        trace!("a scratch buffer of {size} bytes for copies could not be made: {error}");
        return None;
    }
    let buffer = buffer?;
    // The smaller ones stay until the allocator is reset: commands recorded into it
    // name them, and the host does not hold what a command names.
    held.push((buffer.clone(), size));
    Some(buffer)
}

/// The allocator `allocator` was reset or is going, so the GPU is done with its commands:
/// of its scratch buffers only the largest is kept for its next lists, or none if it is
/// `gone`.
pub(crate) fn give_back(allocator: usize, gone: bool) {
    let mut all = SCRATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(all) = all.as_mut() else {
        return;
    };
    if gone {
        all.remove(&allocator);
    } else if let Some(held) = all.get_mut(&allocator) {
        held.sort_by_key(|(_, size)| *size);
        held.drain(..held.len().saturating_sub(1));
    }
}

/// Copies not made so far.
static UNMADE: AtomicU64 = AtomicU64::new(0);

/// Counts a copy that is not made, and says so the first few times and now and then
/// after.
fn unmade(why: &str) {
    let unmade = UNMADE.fetch_add(1, Ordering::Relaxed) + 1;
    if unmade <= 4 || unmade.is_power_of_two() {
        trace!("a copy on a list is not made ({unmade} so far): {why}");
    }
}

/// `CopyBufferRegion`: `(to, offset in it, from, offset in it, bytes)`.
///
/// # Safety
/// `to` and `from` are null or live resources.
unsafe extern "system" fn copy_buffer_region(
    this: &List,
    to: *const Resource,
    to_offset: u64,
    from: *const Resource,
    from_offset: u64,
    bytes: u64,
) {
    // SAFETY: the title's resources, null or live.
    let (Some(to), Some(from)) = (unsafe { to.as_ref() }, unsafe { from.as_ref() }) else {
        unmade("a buffer copy names no resource");
        return;
    };
    copy_range(
        this,
        to.address() + to_offset,
        from.address() + from_offset,
        bytes,
    );
}

/// `CopyMemoryX`: `(to, from, bytes, flags)`. The console's copy between two addresses of
/// title memory with no resource named, which is what a buffer copy comes to here.
extern "system" fn copy_memory(this: &List, to: u64, from: u64, bytes: u64, _flags: u32) {
    if to == 0 || from == 0 {
        unmade("a memory copy names no address");
        return;
    }
    copy_range(this, to, from, bytes);
}

/// Has the host GPU copy `bytes` of title memory from `from_address` to `to_address` at
/// this point of the list.
fn copy_range(this: &List, to_address: u64, from_address: u64, bytes: u64) {
    if bytes == 0 {
        return;
    }
    let (Some(target), Some(source)) = (
        crate::inplace::find(to_address, bytes),
        crate::inplace::find(from_address, bytes),
    ) else {
        unmade(&format!(
            "{bytes} bytes from {from_address:#x} to {to_address:#x} are not each one stretch \
             of one bank of title memory"
        ));
        return;
    };
    let host = this.host();
    // A texture that lies where the copy writes is the title's way of filling it.
    this.note_written(to_address, bytes);
    if target.buffer.as_raw() != source.buffer.as_raw() {
        this.use_buffer(&source.buffer, READ);
        this.use_buffer(&target.buffer, D3D12_RESOURCE_STATE_COPY_DEST);
        // SAFETY: a plain call on the host list, with ranges inside both buffers.
        unsafe {
            host.CopyBufferRegion(
                &target.buffer,
                target.offset,
                &source.buffer,
                source.offset,
                bytes,
            );
        }
        return;
    }
    // Both ranges are in one bank: out to the scratch buffer, and back in.
    let Some(scratch) = scratch(this.allocator_key(), bytes) else {
        unmade("the host gave no scratch buffer");
        return;
    };
    let bank = &source.buffer;
    this.use_buffer(bank, READ);
    // SAFETY: plain calls on the host list, with ranges inside the buffers; the scratch
    // buffer is held for the allocator, whose commands these are.
    unsafe {
        host.CopyBufferRegion(&scratch, 0, bank, source.offset, bytes);
    }
    this.transition(
        &scratch,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    this.use_buffer(bank, D3D12_RESOURCE_STATE_COPY_DEST);
    // SAFETY: as above.
    unsafe {
        host.CopyBufferRegion(bank, target.offset, &scratch, 0, bytes);
    }
    this.transition(
        &scratch,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
}

/// The copy methods by their position in the older list layout.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (15, copy_buffer_region as *const () as usize),
        (60, copy_memory as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_buffer_is_a_power_of_two_no_less_than_the_least() {
        assert_eq!(scratch_size(1), LEAST_SCRATCH);
        assert_eq!(scratch_size(LEAST_SCRATCH), LEAST_SCRATCH);
        assert_eq!(scratch_size(LEAST_SCRATCH + 1), 2 * LEAST_SCRATCH);
        assert_eq!(scratch_size(5 << 20), 8 << 20);
    }

    #[test]
    fn the_copy_has_a_position_of_its_own_among_the_lists_methods() {
        let ours = methods();
        let others: Vec<usize> = crate::record::methods()
            .into_iter()
            .chain(crate::draw::methods())
            .map(|(slot, _)| slot)
            .collect();
        assert!(ours.iter().all(|(slot, _)| !others.contains(slot)));
    }
}
