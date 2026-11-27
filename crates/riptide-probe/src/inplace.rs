//! Title memory as the design has it: a committed section of RAM, mapped for the CPU and
//! opened as a D3D12 heap with one buffer over all of it (open question Q1).

use crate::buffers::{GIB, TILE, buffer_desc, pattern, read_back, upload};
use crate::device::Gpu;
use crate::report::Facts;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Memory::*;
use windows::core::{Interface, PCWSTR};

/// The section and its view. Unmaps and closes on drop, so every error path cleans up.
struct Section {
    handle: HANDLE,
    view: *mut u8,
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: `view` and `handle` came from MapViewOfFile and CreateFileMappingW and are
        // released exactly once, here. A heap opened on the section keeps the memory alive.
        unsafe {
            if !self.view.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            }
            let _ = CloseHandle(self.handle);
        }
    }
}

pub struct InPlace {
    pub view: *mut u8,
    pub bytes: u64,
    pub buffer: ID3D12Resource,
    _heap: ID3D12Heap,
    _section: Section,
}

impl InPlace {
    /// The CPU's view of `len` bytes at `offset`.
    #[allow(clippy::mut_from_ref)]
    pub fn slice(&self, offset: u64, len: u64) -> &mut [u8] {
        assert!(offset + len <= self.bytes);
        // SAFETY: the view maps `bytes` readable and writable bytes for as long as `self`
        // lives; the probe is single-threaded and never holds two slices over one range.
        unsafe { std::slice::from_raw_parts_mut(self.view.add(offset as usize), len as usize) }
    }
}

pub fn open(gpu: &Gpu, bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> windows::core::Result<InPlace> {
    // SAFETY: plain Win32 and COM calls with valid arguments; ownership of the handle and
    // the view passes to `Section` at once.
    unsafe {
        let handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE | SEC_COMMIT,
            (bytes >> 32) as u32,
            bytes as u32,
            PCWSTR::null(),
        )?;
        let mut section = Section { handle, view: std::ptr::null_mut() };
        section.view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize).Value.cast();
        if section.view.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        let device: ID3D12Device3 = gpu.device.cast()?;
        let heap: ID3D12Heap = device.OpenExistingHeapFromFileMapping(handle)?;
        let desc = buffer_desc(bytes, flags | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER);
        let mut buffer: Option<ID3D12Resource> = None;
        gpu.device.CreatePlacedResource(&heap, 0, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut buffer)?;
        Ok(InPlace {
            view: section.view,
            bytes,
            buffer: buffer.expect("CreatePlacedResource succeeded without a resource"),
            _heap: heap,
            _section: section,
        })
    }
}

/// CPU writes the last tile and the GPU copies it out; the GPU copies into the first tile
/// and the CPU reads it. True when both arrive intact.
pub fn round_trip(gpu: &Gpu, mem: &InPlace) -> windows::core::Result<bool> {
    let last = mem.bytes - TILE;
    let from_cpu = pattern(0x11, TILE as usize);
    mem.slice(last, TILE).copy_from_slice(&from_cpu);
    let seen_by_gpu = read_back(gpu, &mem.buffer, last, TILE)?;

    let from_gpu = pattern(0x22, TILE as usize);
    upload(gpu, &mem.buffer, 0, &from_gpu)?;
    let seen_by_cpu = mem.slice(0, TILE).to_vec();

    Ok(seen_by_gpu == from_cpu && seen_by_cpu == from_gpu)
}

fn try_size(gpu: &Gpu, bytes: u64, facts: &mut Facts) -> bool {
    match open(gpu, bytes, D3D12_RESOURCE_FLAG_NONE).and_then(|mem| round_trip(gpu, &mem)) {
        Ok(true) => true,
        Ok(false) => {
            facts.notes.push(format!("in-place {} GiB: data did not arrive intact", bytes / GIB));
            false
        }
        Err(e) => {
            facts.notes.push(format!("in-place {} GiB: {e}", bytes / GIB));
            false
        }
    }
}

pub fn check_in_place(gpu: &Gpu, facts: &mut Facts) {
    facts.in_place_1gib = Some(try_size(gpu, GIB, facts));
    facts.in_place_14gib = Some(try_size(gpu, 14 * GIB, facts));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs the target machine"]
    fn title_sized_block_opens_in_place() {
        let gpu = crate::device::open(false).unwrap();
        let mut facts = Facts::default();
        check_in_place(&gpu, &mut facts);
        assert_eq!(facts.in_place_1gib, Some(true), "notes: {:?}", facts.notes);
        assert_eq!(facts.in_place_14gib, Some(true), "notes: {:?}", facts.notes);
    }
}
