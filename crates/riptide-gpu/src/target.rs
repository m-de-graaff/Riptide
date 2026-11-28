//! Host textures for a title's textures, and the host views a list renders to.
//!
//! A title's texture is a description and an address. The host GPU cannot use the console's
//! layout at that address, so a texture that is drawn to or sampled has a host texture of
//! its own in video memory, made the first time a command needs it. (Filling it from title
//! memory, for textures the title loads, is texture conversion, decision D4, which is not
//! built: so far host textures hold only what is rendered into them.)
//!
//! Render and depth target descriptors are records in title memory (`view.rs`). When a
//! command names one, a host view of the host texture is made in a host descriptor heap and
//! kept for the next time the same record is named.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_DEPTH_STENCIL_VIEW_DESC, D3D12_DESCRIPTOR_HEAP_DESC,
    D3D12_DESCRIPTOR_HEAP_FLAG_NONE, D3D12_DESCRIPTOR_HEAP_TYPE, D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
    D3D12_DESCRIPTOR_HEAP_TYPE_RTV, D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_DEFAULT, D3D12_RENDER_TARGET_VIEW_DESC, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION, D3D12_RESOURCE_FLAGS, D3D12_RESOURCE_STATE_COMMON,
    D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12DescriptorHeap, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use crate::resource::{Description, Resource};
use crate::trace;
use crate::view::{KIND_DEPTH_TARGET, KIND_RENDER_TARGET, Target};

/// The resource flags the host has too: render target, depth target, unordered access.
const HOST_FLAGS: u32 = 0x7;

/// The host's format for a console format. PC's formats are the same numbers. The
/// console's 16-bit depth with stencil has no PC format, and the 24-bit ones stand in for
/// it. `None` for the console's other formats, which have no stand-in yet.
pub(crate) fn host_format(format: u32) -> Option<DXGI_FORMAT> {
    let number = match format {
        118 => 45,
        119 => 46,
        120 => 47,
        116 | 117 | 189..=191 => return None,
        other => other,
    };
    Some(DXGI_FORMAT(number as i32))
}

/// The host description of a title's texture.
pub(crate) fn host_description(description: &Description) -> Option<D3D12_RESOURCE_DESC> {
    Some(D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION(description.dimension as i32),
        Alignment: 0,
        Width: description.width,
        Height: description.height.max(1),
        DepthOrArraySize: description.depth_or_array_size.max(1),
        MipLevels: description.mip_levels,
        Format: host_format(description.format)?,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: description.sample_count.max(1),
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAGS((description.flags & HOST_FLAGS) as i32),
    })
}

/// Makes the host texture for a title's texture, in video memory, in the common state.
pub(crate) fn make(description: &Description) -> Option<ID3D12Resource> {
    if description.dimension < 2 {
        return None;
    }
    let Some(host_description) = host_description(description) else {
        trace!("no host texture for a format the host does not have: {description:?}");
        return None;
    };
    let host = crate::host::host().ok()?;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a plain call on the host device with valid descriptions.
    let result = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &host_description,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut made,
        )
    };
    match (result, made) {
        (Ok(()), Some(texture)) => {
            show(format_args!("host texture made for {description:?}"));
            Some(texture)
        }
        (result, _) => {
            trace!(
                "the host refused a texture ({description:?}): {:?}",
                result.err()
            );
            None
        }
    }
}

/// Says what was made, for the first few.
fn show(line: std::fmt::Arguments<'_>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SHOWN: AtomicU32 = AtomicU32::new(0);
    if SHOWN.fetch_add(1, Ordering::Relaxed) < 8 {
        trace!("{line}");
    }
}

/// How many host views of each kind are kept. When the heap is full the oldest are made
/// over.
const VIEWS: u32 = 4096;

/// The host views of one kind made so far: a host heap, and which record's view is in
/// which place.
struct Views {
    heap: ID3D12DescriptorHeap,
    start: usize,
    step: usize,
    /// What a view was made of, and its place.
    places: HashMap<Key, u32>,
    /// What is in each place, to forget it when the place is made over.
    made: Vec<Option<Key>>,
    next: u32,
}

/// A target record, as far as it decides the host view: the host texture, whether the
/// title described the view, and the description.
type Key = (usize, bool, [u32; 6]);

impl Views {
    fn new(kind: D3D12_DESCRIPTOR_HEAP_TYPE) -> Option<Views> {
        let host = crate::host::host().ok()?;
        let description = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: kind,
            NumDescriptors: VIEWS,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        };
        // SAFETY: plain calls on the host device and the new heap.
        unsafe {
            let heap: ID3D12DescriptorHeap = host.device.CreateDescriptorHeap(&description).ok()?;
            Some(Views {
                start: heap.GetCPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind) as usize,
                heap,
                places: HashMap::new(),
                made: vec![None; VIEWS as usize],
                next: 0,
            })
        }
    }

    /// The place for `key`: where its view already is, or the next place, with `true` if
    /// the view has to be made there.
    fn place(&mut self, key: Key) -> (D3D12_CPU_DESCRIPTOR_HANDLE, bool) {
        let (index, new) = match self.places.get(&key) {
            Some(&index) => (index, false),
            None => {
                let index = self.next;
                self.next = (self.next + 1) % VIEWS;
                if let Some(old) = self.made[index as usize].replace(key) {
                    self.places.remove(&old);
                }
                self.places.insert(key, index);
                (index, true)
            }
        };
        let handle = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.start + index as usize * self.step,
        };
        (handle, new)
    }
}

static RENDER_VIEWS: Mutex<Option<Views>> = Mutex::new(None);
static DEPTH_VIEWS: Mutex<Option<Views>> = Mutex::new(None);

/// A host texture and a host view of it, for a command to render to or clear.
pub(crate) struct HostTarget {
    pub(crate) texture: ID3D12Resource,
    pub(crate) view: D3D12_CPU_DESCRIPTOR_HANDLE,
}

/// The host texture and view for the target record at the descriptor `handle`.
///
/// # Safety
/// `handle` is zero or the address of a target descriptor, and the resource its record
/// names is live.
unsafe fn target(handle: u64, kind: u8) -> Option<HostTarget> {
    // SAFETY: the title's descriptor.
    let record: Target = unsafe { crate::view::target(handle, kind) }?;
    // SAFETY: the record's resource is live, as the caller says.
    let resource = unsafe { (record.resource as *const Resource).as_ref() }?;
    let texture = resource.host()?.clone();
    let host = crate::host::host().ok()?;
    let depth = kind == KIND_DEPTH_TARGET;
    let mut views = if depth { &DEPTH_VIEWS } else { &RENDER_VIEWS }
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if views.is_none() {
        *views = Views::new(if depth {
            D3D12_DESCRIPTOR_HEAP_TYPE_DSV
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_RTV
        });
    }
    let views = views.as_mut()?;
    let key: Key = (
        windows::core::Interface::as_raw(&texture) as usize,
        record.described,
        record.description,
    );
    let (view, new) = views.place(key);
    if new {
        // The title's description has the host's layout. Its format is the console's
        // number, which for the formats the host has is the host's.
        let mut words = record.description;
        if let Some(format) = host_format(words[0]) {
            words[0] = format.0 as u32;
        }
        // SAFETY: plain calls on the host device. The description is 24 bytes of plain
        // numbers in the layout of the host's view descriptions, and outlives the call.
        unsafe {
            if depth {
                let description = record
                    .described
                    .then(|| (&raw const words).cast::<D3D12_DEPTH_STENCIL_VIEW_DESC>());
                host.device
                    .CreateDepthStencilView(&texture, description, view);
            } else {
                let description = record
                    .described
                    .then(|| (&raw const words).cast::<D3D12_RENDER_TARGET_VIEW_DESC>());
                host.device
                    .CreateRenderTargetView(&texture, description, view);
            }
        }
    }
    // The heap lives as long as the process; naming it here keeps that plain.
    let _ = &views.heap;
    Some(HostTarget { texture, view })
}

/// The host texture and view for a render target descriptor.
///
/// # Safety
/// As [`target`].
pub(crate) unsafe fn render_target(handle: u64) -> Option<HostTarget> {
    // SAFETY: as received.
    unsafe { target(handle, KIND_RENDER_TARGET) }
}

/// The host texture and view for a depth target descriptor.
///
/// # Safety
/// As [`target`].
pub(crate) unsafe fn depth_target(handle: u64) -> Option<HostTarget> {
    // SAFETY: as received.
    unsafe { target(handle, KIND_DEPTH_TARGET) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn described(format: u32, flags: u32) -> Description {
        Description {
            dimension: 3,
            alignment: 0,
            width: 3840,
            height: 2160,
            depth_or_array_size: 1,
            mip_levels: 1,
            format,
            sample_count: 0,
            sample_quality: 0,
            layout: 0x11b,
            flags,
        }
    }

    #[test]
    fn a_host_texture_has_the_titles_shape_and_only_the_flags_the_host_knows() {
        let host = host_description(&described(28, 0x30_0005)).unwrap();
        assert_eq!((host.Width, host.Height, host.MipLevels), (3840, 2160, 1));
        assert_eq!(host.Format.0, 28);
        assert_eq!(host.Flags.0, 5);
        assert_eq!(host.SampleDesc.Count, 1);
        assert_eq!(host.Layout, D3D12_TEXTURE_LAYOUT_UNKNOWN);
    }

    #[test]
    fn console_depth_formats_have_host_stand_ins_and_the_others_do_not() {
        assert_eq!(host_format(118).unwrap().0, 45);
        assert_eq!(host_format(120).unwrap().0, 47);
        assert_eq!(host_format(40).unwrap().0, 40);
        assert_eq!(host_format(116), None);
        assert!(host_description(&described(190, 0)).is_none());
        // A buffer has no host texture.
        let buffer = Description {
            dimension: 1,
            ..described(0, 0)
        };
        assert!(make(&buffer).is_none());
    }
}
