//! Filling host textures from title memory (issue #25, the slow and correct way first).
//!
//! On the console a texture is title memory in one of the GPU's own layouts, which the
//! title writes with its CPU or reads into from disc. The host GPU cannot read those
//! layouts, so a texture a shader may read has a host texture, and what the title's
//! memory holds is copied into it: each level of each slice is brought out of the console
//! layout into plain rows (`riptide-formats`) and copied to the host texture.
//!
//! A shader names a texture by a number it reads from memory, so nothing here knows which
//! textures a draw reads. Every texture a view for shaders has been made of is therefore
//! kept filled: it is filled before the first lists are submitted after its view was made,
//! and again whenever the title writes its memory, which a guard on the memory's pages
//! reports (`riptide-mem`). Textures the title draws to or writes from shaders are not
//! filled from memory: on the host their contents live in the host texture alone.
//!
//! The conversion runs on the CPU, on the thread that submits. Doing it on the GPU is the
//! rest of issue #25, to be done when a measurement of this asks for it.

use std::collections::HashMap;
use std::sync::Mutex;

use riptide_formats::texture::{LinearPitch, Texture};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_UPLOAD, D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_RESOURCE_BARRIER,
    D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
    D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_STATE_COPY_DEST,
    D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_STATES, D3D12_RESOURCE_TRANSITION_BARRIER,
    D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
    D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ID3D12CommandAllocator, ID3D12CommandList, ID3D12CommandQueue,
    ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use crate::resource::Resource;
use crate::target::REST;
use crate::trace;

/// The resource flags of a texture whose contents are made on the GPU: a render target, a
/// depth target, or unordered access.
const MADE_ON_THE_GPU: u32 = 0x7;

#[derive(Default)]
struct Kept {
    /// Every texture kept filled, by the address of its resource object.
    textures: HashMap<usize, Watched>,
    /// What an upload used, to let go when the GPU has run it: the number of the
    /// submission that follows it on its queue, by the queue.
    used: Vec<(usize, u64, Used)>,
    /// Textures whose memory a submission's copies write: the queue, the submission's
    /// number on it, and the resource object. They are filled again when it has run.
    pending: Vec<(usize, u64, usize)>,
    /// How many times a copy wrote a texture's memory.
    copied_into: u64,
    filled: u64,
    bytes: u64,
}

/// A texture kept filled.
struct Watched {
    /// Whether it has to be filled again.
    dirty: bool,
    /// Where its memory lies: its first address and how many bytes. No bytes if its
    /// layout in memory is not known.
    lies: (u64, u64),
}

/// The host objects one upload used.
struct Used {
    _allocator: ID3D12CommandAllocator,
    _list: ID3D12GraphicsCommandList,
    _buffer: ID3D12Resource,
}

// SAFETY: free-threaded host objects, kept only to be dropped.
unsafe impl Send for Used {}

static KEPT: Mutex<Option<Kept>> = Mutex::new(None);

fn kept<T>(work: impl FnOnce(&mut Kept) -> T) -> T {
    let mut kept = KEPT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    work(kept.get_or_insert_with(Kept::default))
}

/// Has the texture of `resource` kept filled from its memory, if its contents are the
/// title memory's. Called when a view for shaders is made of it.
pub(crate) fn watch(resource: &Resource) {
    let description = resource.description();
    if description.dimension < 2
        || description.flags & MADE_ON_THE_GPU != 0
        || description.sample_count > 1
    {
        return;
    }
    kept(|kept| {
        let before = kept.textures.len();
        kept.textures
            .entry(std::ptr::from_ref(resource) as usize)
            .or_insert_with(|| Watched {
                dirty: true,
                lies: (
                    resource.address(),
                    crate::resource::console_texture(description)
                        .map_or(0, |layout| layout.bytes()),
                ),
            });
        let watched = kept.textures.len();
        if watched != before && watched.is_power_of_two() && watched >= 32 {
            trace!("{watched} textures are kept filled from title memory");
        }
    });
}

/// Whether two ranges, each a first address and a length, share a byte.
fn overlap(one: (u64, u64), other: (u64, u64)) -> bool {
    one.0 < other.0.saturating_add(other.1) && other.0 < one.0.saturating_add(one.1)
}

/// Notes that submission `number` on `queue` has the GPU write the ranges `written` of
/// title memory. A texture kept filled that lies in one of them is filled again once the
/// submission has run: the title streams a texture in elsewhere and has the GPU copy it
/// to where the texture lies, which no guard on the CPU's writes hears.
pub(crate) fn written_by_gpu(queue: &ID3D12CommandQueue, number: u64, written: &[(u64, u64)]) {
    let key = queue.as_raw() as usize;
    kept(|kept| {
        let Kept {
            textures,
            pending,
            copied_into,
            ..
        } = kept;
        for (&address, watched) in textures.iter() {
            if written.iter().any(|&range| overlap(range, watched.lies)) {
                pending.push((key, number, address));
                *copied_into += 1;
                if *copied_into == 1 || copied_into.is_power_of_two() {
                    trace!(
                        "{copied_into} times so far a copy on a list wrote a texture's memory;                          the texture is filled again when the copy has run"
                    );
                }
            }
        }
    });
}

/// Notes that what is mapped at the title addresses of `range` has changed. A texture
/// that lies there is filled again: a title maps a texture it streams a piece at a time,
/// and each piece shows once it is there.
pub(crate) fn remapped(range: &std::ops::Range<u64>) {
    let range = (range.start, range.end - range.start);
    kept(|kept| {
        for watched in kept.textures.values_mut() {
            if overlap(range, watched.lies) {
                watched.dirty = true;
            }
        }
    });
}

/// Forgets the texture of the resource object at `resource`, which is going.
pub(crate) fn forget(resource: usize) {
    let watched = kept(|kept| {
        kept.pending.retain(|&(_, _, pending)| pending != resource);
        kept.textures.remove(&resource).is_some()
    });
    if watched && let Some(services) = crate::services() {
        services.memory.unguard(resource as u64);
    }
}

/// A transition of all of `texture`.
fn transition(
    texture: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
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
    }
}

/// One texture's levels brought out of title memory: where each lies in the upload buffer.
struct Filled {
    texture: ID3D12Resource,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
}

/// Brings every level of every slice of `resource` out of its memory into plain rows,
/// appended to `rows`. `None` if the texture cannot be read now: its memory is not all
/// mapped, or its layout is not known.
fn bring_out(resource: &Resource, rows: &mut Vec<u8>) -> Option<Filled> {
    let services = crate::services()?;
    let host = crate::host::host().ok()?;
    let texture = resource.host()?.clone();
    let layout: Texture = crate::resource::console_texture(resource.description())?;
    let bytes = layout.bytes();
    // What is mapped of the texture's memory, with zeros where nothing is: a texture the
    // title streams has its smaller levels first, and the others show black until they
    // come. The title may be writing the memory: what is read then is some mix of old and
    // new, and the guard that was set before this says it was written, so it is read
    // again.
    let mut memory = vec![0u8; usize::try_from(bytes).ok()?];
    if services.memory.read_mapped(resource.address(), &mut memory) == 0 {
        return None;
    }
    let memory = memory.as_slice();
    // SAFETY: a plain call on the host texture.
    let description = unsafe { texture.GetDesc() };
    let three = resource.description().dimension == 4;
    let mips = layout.mips();
    let slices = if three {
        1
    } else {
        u32::from(resource.description().depth_or_array_size).max(1)
    };
    let count = mips * slices;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut row_counts = vec![0u32; count as usize];
    let mut total = 0u64;
    // SAFETY: a plain call on the host device with room for `count` answers of each kind.
    unsafe {
        host.device.GetCopyableFootprints(
            &description,
            0,
            count,
            rows.len().next_multiple_of(512) as u64,
            Some(footprints.as_mut_ptr()),
            Some(row_counts.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let start = rows.len().next_multiple_of(512);
    rows.resize(start + total as usize, 0);
    for slice in 0..slices {
        for mip in 0..mips {
            let index = (mip + slice * mips) as usize;
            let footprint = &footprints[index];
            let pitch = LinearPitch {
                row: footprint.Footprint.RowPitch as usize,
                depth: footprint.Footprint.RowPitch as usize * row_counts[index] as usize,
            };
            let into = rows.get_mut(footprint.Offset as usize..)?;
            if let Err(error) = layout.detile(memory, 0, mip, slice, into, pitch) {
                show_unreadable(resource, &format!("{error:?}"));
                return None;
            }
        }
    }
    Some(Filled {
        texture,
        footprints,
    })
}

/// Says that a texture could not be brought out of its memory, for the first few.
fn show_unreadable(resource: &Resource, why: &str) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SHOWN: AtomicU32 = AtomicU32::new(0);
    if SHOWN.fetch_add(1, Ordering::Relaxed) < 8 {
        trace!(
            "a texture is not filled from its memory ({why}): {:?}",
            resource.description()
        );
    }
}

/// Fills every texture that has to be filled, on `queue`, before whatever is submitted to
/// it next. `number` is the number the next submission of lists on this queue will have,
/// and `done` the number of the last one the GPU has finished: what earlier uploads used
/// is let go by them.
pub(crate) fn flush(queue: &ID3D12CommandQueue, number: u64, done: u64) {
    let Some(services) = crate::services() else {
        return;
    };
    let written = services.memory.take_written();
    let key = queue.as_raw() as usize;
    let dirty: Vec<usize> = kept(|kept| {
        kept.used
            .retain(|(queue, used_at, _)| *queue != key || *used_at > done);
        for resource in written {
            if let Some(watched) = kept.textures.get_mut(&(resource as usize)) {
                watched.dirty = true;
            }
        }
        // What the GPU was to write, it has written when its submission is done.
        let mut pending = std::mem::take(&mut kept.pending);
        pending.retain(|&(queue, after, resource)| {
            let ran = queue == key && after <= done;
            if ran && let Some(watched) = kept.textures.get_mut(&resource) {
                watched.dirty = true;
            }
            !ran
        });
        kept.pending = pending;
        kept.textures
            .iter()
            .filter(|(_, watched)| watched.dirty)
            .map(|(&resource, _)| resource)
            .collect()
    });
    if dirty.is_empty() {
        return;
    }
    let mut rows = Vec::new();
    let mut filled = Vec::new();
    for &address in &dirty {
        // SAFETY: a watched resource is live: it is forgotten here before it goes.
        let resource = unsafe { &*(address as *const Resource) };
        let bytes = crate::resource::console_texture(resource.description())
            .map_or(0, |layout| layout.bytes());
        // The guard goes up before the memory is read, so that a write during the read
        // is heard. Memory that is not mapped yet cannot be guarded, and the texture
        // stays to be filled.
        if bytes == 0 {
            show_unreadable(resource, "its layout in memory is not known");
            continue;
        }
        // What of it is mapped is watched; none of it mapped yet is nothing to fill, and
        // it is filled when some of it is (see `remapped`).
        if services
            .memory
            .guard_mapped(resource.address(), bytes, address as u64)
            .is_err()
        {
            kept(|kept| {
                if let Some(watched) = kept.textures.get_mut(&address) {
                    watched.dirty = false;
                }
            });
            continue;
        }
        if let Some(one) = bring_out(resource, &mut rows) {
            filled.push((address, one));
        }
    }
    if filled.is_empty() {
        return;
    }
    match copy(queue, &rows, &filled) {
        Ok(used) => kept(|kept| {
            for (address, _) in &filled {
                if let Some(watched) = kept.textures.get_mut(address) {
                    watched.dirty = false;
                }
            }
            kept.used.push((key, number, used));
            let before = kept.filled;
            kept.filled += filled.len() as u64;
            kept.bytes += rows.len() as u64;
            // Said the first time, and each time the count passes a power of two.
            if before == 0 || kept.filled.ilog2() != before.ilog2() {
                trace!(
                    "{} textures filled from title memory so far, {} MiB",
                    kept.filled,
                    kept.bytes >> 20
                );
            }
        }),
        Err(error) => trace!("textures could not be filled from title memory: {error}"),
    }
}

/// Copies `rows` to the host textures of `filled`, on `queue`.
fn copy(
    queue: &ID3D12CommandQueue,
    rows: &[u8],
    filled: &[(usize, Filled)],
) -> windows::core::Result<Used> {
    let host = crate::host::host()
        .map_err(|code| windows::core::Error::from_hresult(windows::core::HRESULT(code)))?;
    let device = &host.device;
    // SAFETY: plain calls on the host device and objects made here. The buffer is written
    // inside its size, and it, the list and the allocator are kept until the GPU has run
    // the list.
    unsafe {
        let mut buffer: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: rows.len() as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut buffer,
        )?;
        let buffer = buffer.ok_or_else(windows::core::Error::empty)?;
        let mut mapped = std::ptr::null_mut();
        buffer.Map(0, None, Some(&mut mapped))?;
        std::ptr::copy_nonoverlapping(rows.as_ptr(), mapped.cast::<u8>(), rows.len());
        buffer.Unmap(0, None);

        let allocator: ID3D12CommandAllocator =
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
        let to: Vec<D3D12_RESOURCE_BARRIER> = filled
            .iter()
            .map(|(_, one)| transition(&one.texture, REST, D3D12_RESOURCE_STATE_COPY_DEST))
            .collect();
        list.ResourceBarrier(&to);
        for (_, one) in filled {
            for (index, footprint) in one.footprints.iter().enumerate() {
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&buffer),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: *footprint,
                    },
                };
                let into = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&one.texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: index as u32,
                    },
                };
                list.CopyTextureRegion(&into, 0, 0, 0, &from, None);
            }
        }
        let back: Vec<D3D12_RESOURCE_BARRIER> = filled
            .iter()
            .map(|(_, one)| transition(&one.texture, D3D12_RESOURCE_STATE_COPY_DEST, REST))
            .collect();
        list.ResourceBarrier(&back);
        list.Close()?;
        queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>()?)]);
        Ok(Used {
            _allocator: allocator,
            _list: list,
            _buffer: buffer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_share_a_byte_or_do_not() {
        assert!(overlap((100, 10), (109, 1)));
        assert!(overlap((100, 10), (90, 11)));
        assert!(overlap((100, 10), (0, u64::MAX)));
        // One ends where the other begins: no byte is shared.
        assert!(!overlap((100, 10), (110, 5)));
        assert!(!overlap((100, 10), (90, 10)));
        assert!(!overlap((100, 0), (100, 10)));
    }
}
