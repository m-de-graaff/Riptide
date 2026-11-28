//! The two host heaps every translated shader reads from (design T6 and T7).
//!
//! A title's descriptor is a record in title memory, which a shader reads where it lies.
//! What a record cannot hold is a host texture or a host sampler, so it holds a place in
//! one of two host heaps instead, and the shader takes the texture or sampler from that
//! place. The heaps are made once, are the only ones ever set on a host list, and are laid
//! out as `docs/design/2026-11-28-shader-translation.md` says under "The host shader's
//! bindings":
//!
//! | Places | Hold |
//! | --- | --- |
//! | 0 to 15 | each bank of title memory as a buffer of words, to read |
//! | 16 to 31 | the same, to write |
//! | 32 to 47 | a view of nothing for each kind of texture, by the console's kind number |
//! | 64 on | the host views of the title's textures |
//!
//! Equal views of one texture are one place, because a title copies records about and
//! makes the same view many times. A place is kept until its texture goes.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_BUFFER_SRV, D3D12_BUFFER_SRV_FLAG_RAW, D3D12_BUFFER_UAV, D3D12_BUFFER_UAV_FLAG_RAW,
    D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
    D3D12_DESCRIPTOR_HEAP_DESC, D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, D3D12_DESCRIPTOR_HEAP_TYPE,
    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, D3D12_SAMPLER_DESC,
    D3D12_SHADER_RESOURCE_VIEW_DESC, D3D12_SHADER_RESOURCE_VIEW_DESC_0, D3D12_SRV_DIMENSION,
    D3D12_SRV_DIMENSION_BUFFER, D3D12_UAV_DIMENSION_BUFFER, D3D12_UNORDERED_ACCESS_VIEW_DESC,
    D3D12_UNORDERED_ACCESS_VIEW_DESC_0, ID3D12DescriptorHeap, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_TYPELESS,
};

use crate::trace;

/// How many places the resource heap has.
const PLACES: u32 = 1 << 19;
/// How many banks of title memory have places.
pub(crate) const BANKS: u32 = 16;
/// The first place of the views of nothing. The view for a kind of texture is at this
/// plus the kind's number, less 8: kinds run from 8 to 15.
pub(crate) const NOTHING: u32 = 32;
/// The first place a texture's view can have.
pub(crate) const FIRST_TEXTURE: u32 = 64;
/// How many places the sampler heap has: all the host allows.
const SAMPLERS: u32 = 2048;
/// A sampler's comparing form is this many places after its plain form.
pub(crate) const COMPARING: u32 = SAMPLERS / 2;

/// What a host view of a texture is made of: the resource object, whether it is for
/// unordered access, and the view description in the host's layout (ten words, all zero
/// when the title gave none).
pub(crate) type ViewKey = (usize, bool, [u32; 10]);

struct Heaps {
    resources: ID3D12DescriptorHeap,
    resource_start: usize,
    resource_step: usize,
    samplers: ID3D12DescriptorHeap,
    sampler_start: usize,
    sampler_step: usize,
    /// Which banks have their two places filled.
    banks: [bool; BANKS as usize],
    views: HashMap<ViewKey, u32>,
    /// Places whose texture has gone, to use again.
    free: Vec<u32>,
    /// The next place never used yet.
    next: u32,
    sampler_places: HashMap<[u32; 13], u32>,
}

// SAFETY: the heaps are free-threaded host objects, and everything else is plain data
// behind the lock the heaps are kept in.
unsafe impl Send for Heaps {}

static HEAPS: Mutex<Option<Option<Heaps>>> = Mutex::new(None);

fn heap(
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
    count: u32,
    visible: bool,
) -> Option<ID3D12DescriptorHeap> {
    let host = crate::host::host().ok()?;
    let description = D3D12_DESCRIPTOR_HEAP_DESC {
        Type: kind,
        NumDescriptors: count,
        Flags: if visible {
            D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE
        } else {
            D3D12_DESCRIPTOR_HEAP_FLAG_NONE
        },
        NodeMask: 0,
    };
    // SAFETY: a plain call on the host device.
    unsafe { host.device.CreateDescriptorHeap(&description).ok() }
}

/// The description of a view of no texture of the console kind `kind` (8 to 15).
fn nothing(kind: u32) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    // One dimension, two, three, cube, then the arrays of one and two, then multisampled
    // and its array: the host's dimension numbers for the console's kinds 8 to 15.
    let dimension = match kind {
        8 => 2,
        9 => 4,
        10 => 8,
        11 => 9,
        12 => 3,
        13 => 5,
        14 => 6,
        _ => 7,
    };
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION(dimension),
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0::default(),
    }
}

/// A sampler that samples the nearest texel and repeats: what an unwritten place holds.
fn plain_sampler() -> [u32; 13] {
    let mut words = [0u32; 13];
    // Addresses wrap; the comparison is "never"; the level range is everything.
    (words[1], words[2], words[3]) = (1, 1, 1);
    words[6] = 1;
    words[12] = f32::MAX.to_bits();
    words
}

fn make() -> Option<Heaps> {
    let host = crate::host::host().ok()?;
    let resources = heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, PLACES, true)?;
    let samplers = heap(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, SAMPLERS, true)?;
    // SAFETY: plain calls on the host device and the new heaps. Every place written is
    // inside its heap.
    let heaps = unsafe {
        let heaps = Heaps {
            resource_start: resources.GetCPUDescriptorHandleForHeapStart().ptr,
            resource_step: host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize,
            sampler_start: samplers.GetCPUDescriptorHandleForHeapStart().ptr,
            sampler_step: host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER)
                as usize,
            resources,
            samplers,
            banks: [false; BANKS as usize],
            views: HashMap::new(),
            free: Vec::new(),
            next: FIRST_TEXTURE,
            sampler_places: HashMap::new(),
        };
        // Every place a shader can name holds something the host can read: a record that
        // was never written, or was written over with other bytes, names a place by
        // chance. So the whole resource heap starts as views of no texture, copied a block
        // at a time from a heap the CPU can read.
        const BLOCK: u32 = 4096;
        let block = heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, BLOCK, false)?;
        let block_start = block.GetCPUDescriptorHandleForHeapStart();
        let flat = nothing(9);
        for index in 0..BLOCK as usize {
            host.device.CreateShaderResourceView(
                None,
                Some(&flat),
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: block_start.ptr + index * heaps.resource_step,
                },
            );
        }
        for first in (0..PLACES).step_by(BLOCK as usize) {
            host.device.CopyDescriptorsSimple(
                BLOCK.min(PLACES - first),
                heaps.resource(first),
                block_start,
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            );
        }
        for kind in 8..16 {
            host.device.CreateShaderResourceView(
                None,
                Some(&nothing(kind)),
                heaps.resource(NOTHING + kind - 8),
            );
        }
        let plain = plain_sampler();
        for place in 0..SAMPLERS {
            host.device.CreateSampler(
                (&raw const plain).cast::<D3D12_SAMPLER_DESC>(),
                heaps.sampler(place),
            );
        }
        heaps
    };
    trace!("the host's heaps for shaders are made: {PLACES} views and {SAMPLERS} samplers");
    Some(heaps)
}

impl Heaps {
    fn resource(&self, place: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.resource_start + place as usize * self.resource_step,
        }
    }

    fn sampler(&self, place: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.sampler_start + place as usize * self.sampler_step,
        }
    }
}

fn with<T>(work: impl FnOnce(&mut Heaps) -> Option<T>) -> Option<T> {
    let mut heaps = HEAPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    work(heaps.get_or_insert_with(make).as_mut()?)
}

/// The two heaps, to set on a host list: views, then samplers.
#[allow(dead_code)] // the first user is a draw with translated shaders
pub(crate) fn heaps() -> Option<[Option<ID3D12DescriptorHeap>; 2]> {
    with(|heaps| Some([Some(heaps.resources.clone()), Some(heaps.samplers.clone())]))
}

/// Gives every bank of title memory that exists its two places. Called before lists are
/// submitted, since the title's memory grows a bank at a time.
pub(crate) fn sync() {
    let Some(services) = crate::services() else {
        return;
    };
    let count = services.memory.bank_count().min(BANKS as usize);
    with(|heaps| {
        for bank in 0..count {
            if heaps.banks[bank] {
                continue;
            }
            let Some(buffer) = crate::inplace::bank(bank) else {
                continue;
            };
            let words = (riptide_mem::BANK_BYTES / 4) as u32;
            let read = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: 0,
                        NumElements: words,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            let write = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        FirstElement: 0,
                        NumElements: words,
                        StructureByteStride: 0,
                        CounterOffsetInBytes: 0,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    },
                },
            };
            let host = crate::host::host().ok()?;
            // SAFETY: plain calls on the host device with the bank's buffer and places
            // inside the heap.
            unsafe {
                host.device.CreateShaderResourceView(
                    &buffer,
                    Some(&read),
                    heaps.resource(bank as u32),
                );
                host.device.CreateUnorderedAccessView(
                    &buffer,
                    None,
                    Some(&write),
                    heaps.resource(BANKS + bank as u32),
                );
            }
            heaps.banks[bank] = true;
            trace!("bank {bank} of title memory has its places in the shaders' heap");
        }
        Some(())
    });
}

/// The place of the host view `key` describes of `texture`, made if it is new. `None` if
/// the heaps could not be made or are full.
pub(crate) fn view(key: ViewKey, texture: &ID3D12Resource) -> Option<u32> {
    let host = crate::host::host().ok()?;
    with(|heaps| {
        if let Some(&place) = heaps.views.get(&key) {
            return Some(place);
        }
        let place = match heaps.free.pop() {
            Some(place) => place,
            None if heaps.next < PLACES => {
                heaps.next += 1;
                heaps.next - 1
            }
            None => {
                trace!("the shaders' heap is full: a view is not made");
                return None;
            }
        };
        let (_, unordered, words) = &key;
        let described = *words != [0; 10];
        // SAFETY: plain calls on the host device. The description is 40 bytes of plain
        // numbers in the layout of the host's view descriptions, and outlives the call.
        unsafe {
            if *unordered {
                host.device.CreateUnorderedAccessView(
                    texture,
                    None,
                    described
                        .then(|| (&raw const *words).cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()),
                    heaps.resource(place),
                );
            } else {
                host.device.CreateShaderResourceView(
                    texture,
                    described
                        .then(|| (&raw const *words).cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()),
                    heaps.resource(place),
                );
            }
        }
        heaps.views.insert(key, place);
        Some(place)
    })
}

/// Forgets every view of the resource object at `resource`, which is going. Their places
/// become views of nothing and can be used again.
pub(crate) fn forget(resource: usize) {
    let mut heaps = HEAPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(Some(heaps)) = heaps.as_mut() else {
        return;
    };
    let Ok(host) = crate::host::host() else {
        return;
    };
    let gone: Vec<ViewKey> = heaps
        .views
        .keys()
        .filter(|key| key.0 == resource)
        .copied()
        .collect();
    for key in gone {
        if let Some(place) = heaps.views.remove(&key) {
            // SAFETY: a plain call on the host device with a place inside the heap.
            unsafe {
                host.device.CreateShaderResourceView(
                    None,
                    Some(&nothing(9)),
                    heaps.resource(place),
                );
            }
            heaps.free.push(place);
        }
    }
}

/// The place of `sampler` (a host sampler description as 13 words) in the sampler heap,
/// made if it is new. Its comparing form is [`COMPARING`] places further on. When more
/// distinct samplers are asked for than there are places, the first place is given.
pub(crate) fn sampler(sampler: &[u32; 13]) -> u32 {
    let Ok(host) = crate::host::host() else {
        return 0;
    };
    with(|heaps| {
        if let Some(&place) = heaps.sampler_places.get(sampler) {
            return Some(place);
        }
        // Place 0 stays the plain sampler, for records that name none.
        let place = heaps.sampler_places.len() as u32 + 1;
        if place >= COMPARING {
            trace!("more than {COMPARING} distinct samplers: one is not made");
            return Some(0);
        }
        let (plain, comparing) = forms(sampler);
        // SAFETY: plain calls on the host device with places inside the heap; a host
        // sampler description is these 13 words.
        unsafe {
            host.device.CreateSampler(
                (&raw const plain).cast::<D3D12_SAMPLER_DESC>(),
                heaps.sampler(place),
            );
            host.device.CreateSampler(
                (&raw const comparing).cast::<D3D12_SAMPLER_DESC>(),
                heaps.sampler(place + COMPARING),
            );
        }
        heaps.sampler_places.insert(*sampler, place);
        Some(place)
    })
    .unwrap_or(0)
}

/// The two forms of a sampler: one that samples and one that compares. On the console
/// whether a sample compares is the instruction's to say, and every sampler has a
/// comparison; on the host it is the sampler's, so each sampler is made both ways.
fn forms(sampler: &[u32; 13]) -> ([u32; 13], [u32; 13]) {
    /// Where a host filter says how samples are combined: 0 plain, 1 compared, 2 and 3
    /// the least and the greatest.
    const COMBINE: u32 = 3 << 7;
    const COMPARED: u32 = 1 << 7;
    let (mut plain, mut comparing) = (*sampler, *sampler);
    match sampler[0] & COMBINE {
        0 => comparing[0] |= COMPARED,
        COMPARED => plain[0] &= !COMBINE,
        // The least or greatest of the samples has no comparing form: both are it.
        _ => {}
    }
    // A comparison of zero is none at all, which the host refuses in a comparing sampler:
    // "less or equal" is what the console has when the title set none.
    if comparing[6] == 0 {
        comparing[6] = 4;
    }
    (plain, comparing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sampler_has_a_plain_and_a_comparing_form() {
        let mut sampler = plain_sampler();
        sampler[0] = 0x95;
        sampler[6] = 0;
        let (plain, comparing) = forms(&sampler);
        assert_eq!(plain[0], 0x15);
        assert_eq!(comparing[0], 0x95);
        assert_eq!(comparing[6], 4);
        assert_eq!(plain[1..], sampler[1..]);
        // A sampler that takes the greatest of its samples stays one, both ways.
        sampler[0] = 0x180 | 0x15;
        let (plain, comparing) = forms(&sampler);
        assert_eq!((plain[0], comparing[0]), (0x195, 0x195));
        // A plain one gets a comparing form.
        sampler[0] = 0x15;
        assert_eq!(forms(&sampler).1[0], 0x95);
    }

    #[test]
    fn the_places_are_where_the_design_says() {
        assert_eq!(
            (BANKS, NOTHING, FIRST_TEXTURE, COMPARING),
            (16, 32, 64, 1024)
        );
        // Every kind of texture has its view of nothing, in a host dimension of its own.
        let dimensions: Vec<i32> = (8..16).map(|kind| nothing(kind).ViewDimension.0).collect();
        assert_eq!(dimensions, [2, 4, 8, 9, 3, 5, 6, 7]);
    }
}
