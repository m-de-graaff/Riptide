//! The title's memory: one committed block of RAM that is also a D3D12 heap, its allocator,
//! write guards, and console-to-host address conversion. See README.md in this folder.

mod block;
mod budget;
mod guard;
mod pages;

pub use budget::{Pressure, RamBudget, RamUse, free_ram};
pub use pages::{GRANULARITY, MemError, PAGE, Region, State};

use block::Block;
use pages::PageMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
    D3D12_RESOURCE_FLAGS, D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
    ID3D12Device, ID3D12Device3, ID3D12Heap, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::Interface;

/// Where title memory lies. Copy it to wherever addresses are converted; a shader gets `base`
/// as a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressMap {
    pub base: u64,
    pub size: u64,
}

impl AddressMap {
    /// The offset in the one buffer of a title address: one subtraction (decision D2).
    /// An address outside title memory gives an offset of `size` or more.
    #[inline]
    pub const fn offset(&self, address: u64) -> u64 {
        address.wrapping_sub(self.base)
    }

    /// The offset of `address`, or `None` if it is outside title memory.
    #[inline]
    pub const fn checked_offset(&self, address: u64) -> Option<u64> {
        let offset = self.offset(address);
        if offset < self.size {
            Some(offset)
        } else {
            None
        }
    }

    /// The title address of an offset in the buffer.
    #[inline]
    pub const fn address(&self, offset: u64) -> u64 {
        self.base.wrapping_add(offset)
    }
}

/// Title memory opened for the GPU: the heap over the block, and the one buffer over all of
/// it. Offset `n` of the buffer is the byte at title address `base + n`.
pub struct InPlaceHeap {
    pub heap: ID3D12Heap,
    pub buffer: ID3D12Resource,
}

/// All of a title's memory, and the allocator its memory calls go through.
///
/// A title's addresses are host addresses inside the block: the title's code runs natively
/// and uses them as pointers. Pages are inaccessible until committed, as on the console.
pub struct TitleMemory {
    block: Block,
    map: Mutex<PageMap>,
    /// How many bytes are committed right now. Only changed while holding `map`.
    committed: AtomicU64,
}

impl Drop for TitleMemory {
    fn drop(&mut self) {
        guard::forget(self.span());
    }
}

impl TitleMemory {
    /// Creates a block of `bytes`, a non-zero multiple of [`GRANULARITY`]. All of it is
    /// charged against the page file; `MemError::Os` carries Windows' reason if it will not fit.
    pub fn create(bytes: u64) -> Result<TitleMemory, MemError> {
        if bytes == 0 || !bytes.is_multiple_of(GRANULARITY) {
            return Err(MemError::InvalidParameter);
        }
        Ok(TitleMemory {
            block: Block::create(bytes)?,
            map: Mutex::new(PageMap::new(bytes)),
            committed: AtomicU64::new(0),
        })
    }

    pub fn address_map(&self) -> AddressMap {
        AddressMap {
            base: self.block.base(),
            size: self.block.bytes(),
        }
    }

    fn offset(&self, address: u64) -> Result<u64, MemError> {
        self.address_map()
            .checked_offset(address)
            .ok_or(MemError::InvalidAddress)
    }

    fn map(&self) -> MutexGuard<'_, PageMap> {
        // The map is changed in single steps, so it is whole even after a panic elsewhere.
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reserves `size` bytes of addresses and returns where they start. With `address`, the
    /// reservation starts there, rounded down to [`GRANULARITY`]; without, at the lowest free
    /// place. Reserved pages stay inaccessible until committed.
    pub fn reserve(&self, address: Option<u64>, size: u64) -> Result<u64, MemError> {
        let at = address.map(|a| self.offset(a)).transpose()?;
        let range = self.map().reserve(at, size)?;
        Ok(self.block.base() + range.start)
    }

    /// Commits the pages covering `size` bytes at `address`, which must lie inside one
    /// reservation, and returns the first page's address. New pages read as zero; pages
    /// already committed keep their bytes.
    pub fn commit(&self, address: u64, size: u64) -> Result<u64, MemError> {
        let at = self.offset(address)?;
        let mut map = self.map();
        let (range, runs) = map.commit(at, size)?;
        for run in runs {
            let bytes = run.range.end - run.range.start;
            self.block.open(run.range)?;
            self.committed.fetch_add(bytes, Ordering::Relaxed);
        }
        Ok(self.block.base() + range.start)
    }

    /// Reserves and commits `size` bytes at the lowest free place.
    pub fn allocate(&self, size: u64) -> Result<u64, MemError> {
        let address = self.reserve(None, size)?;
        match self.commit(address, size) {
            Ok(_) => Ok(address),
            Err(e) => {
                let _ = self.release(address);
                Err(e)
            }
        }
    }

    /// Decommits the pages covering `size` bytes at `address`, which must lie inside one
    /// reservation. A `size` of zero means the whole reservation that starts at `address`.
    /// The pages become inaccessible and read as zero when committed again.
    pub fn decommit(&self, address: u64, size: u64) -> Result<(), MemError> {
        let at = self.offset(address)?;
        let mut map = self.map();
        for run in map.decommit(at, size)? {
            self.give_back(run)?;
        }
        Ok(())
    }

    /// Frees the reservation that starts at `address`, decommitting whatever is committed.
    pub fn release(&self, address: u64) -> Result<(), MemError> {
        let at = self.offset(address)?;
        let mut map = self.map();
        for run in map.release(at)?.1 {
            self.give_back(run)?;
        }
        Ok(())
    }

    /// Takes committed pages, given as offsets, away from the title: ends their guards,
    /// gives their RAM back to Windows and makes them inaccessible.
    fn give_back(&self, run: Range<u64>) -> Result<(), MemError> {
        let bytes = run.end - run.start;
        self.end_guards(&run);
        self.block.close(run)?;
        self.committed.fetch_sub(bytes, Ordering::Relaxed);
        Ok(())
    }

    /// How many bytes of title memory are committed right now.
    pub fn committed_bytes(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }

    /// The RAM this process uses now: title memory that is committed, and everything else
    /// the process has allocated. This is what the RAM budget is held against.
    pub fn ram_use(&self) -> RamUse {
        RamUse {
            title: self.committed_bytes(),
            private: budget::private_bytes(),
        }
    }

    /// The memory figures `riptide-trace` asks for at the end of a frame. `video_bytes`
    /// comes from `riptide-gpu`, which accounts for video memory.
    pub fn sample(&self, video_bytes: u64) -> riptide_trace::Sample {
        riptide_trace::Sample {
            private_bytes: self.ram_use().total(),
            video_bytes,
        }
    }

    /// The block's addresses.
    fn span(&self) -> Range<u64> {
        let base = self.block.base();
        base..base + self.block.bytes()
    }

    /// Ends the guards on pages that are about to be decommitted, given as offsets.
    fn end_guards(&self, run: &Range<u64>) {
        let base = self.block.base();
        guard::end_overlapping(base, base + run.start..base + run.end);
    }

    /// Write-protects the pages covering `size` bytes at `address` and guards them for
    /// `resource`, a number of the caller's choosing (decisions D3 and D4).
    ///
    /// The first write to any of those pages takes the protection off all of them, puts
    /// `resource` in the list [`TitleMemory::take_written`] returns, and then goes on as if
    /// nothing had happened. Reads are never disturbed.
    ///
    /// Pages are protected whole. A write beside the resource but on one of its pages ends
    /// the guard too: it is reported as written though it was not, which costs a copy and
    /// never a wrong picture. Two resources that share a page are both reported.
    ///
    /// Every page has to be committed. `resource` may be guarded once at a time.
    pub fn guard(&self, address: u64, size: u64, resource: u64) -> Result<(), MemError> {
        if size == 0 {
            return Err(MemError::InvalidParameter);
        }
        let start = self.offset(address)?;
        let end = start
            .checked_add(size)
            .filter(|&end| end <= self.block.bytes())
            .ok_or(MemError::InvalidAddress)?;
        // Holding the map keeps the pages committed until they are guarded.
        let map = self.map();
        let mut at = start;
        while at < end {
            let region = map.query(at)?;
            if region.state != State::Committed {
                return Err(MemError::InvalidAddress);
            }
            at = region.base + region.size;
        }
        let base = self.block.base();
        if guard::guard(self.span(), resource, base + start..base + end) {
            Ok(())
        } else {
            Err(MemError::InvalidParameter)
        }
    }

    /// Ends the guard for `resource` and makes its pages writable again, without reporting
    /// it as written. Returns whether it was guarded: false if a write, or a decommit,
    /// ended the guard first.
    pub fn unguard(&self, resource: u64) -> bool {
        guard::unguard(self.block.base(), resource)
    }

    /// The resources whose guard was ended by a write, or by their pages being decommitted,
    /// since the last call. Each appears once, in the order it happened.
    pub fn take_written(&self) -> Vec<u64> {
        guard::take_written(self.block.base())
    }

    /// How many resources are guarded right now.
    pub fn guard_count(&self) -> usize {
        guard::count(self.block.base())
    }

    /// The run of pages around `address` that share one state.
    pub fn query(&self, address: u64) -> Result<Region, MemError> {
        let base = self.block.base();
        let region = self.map().query(self.offset(address)?)?;
        Ok(Region {
            base: base + region.base,
            allocation_base: region.allocation_base.map(|a| base + a),
            ..region
        })
    }

    /// Opens the block as a D3D12 heap with one buffer over all of it (decision D1).
    ///
    /// `flags` are added to `ALLOW_CROSS_ADAPTER`, which a buffer in such a heap must have.
    /// The GPU reads and writes every page of the block, committed or not: page protection
    /// applies to the CPU only.
    pub fn open_heap(
        &self,
        device: &ID3D12Device,
        flags: D3D12_RESOURCE_FLAGS,
    ) -> windows::core::Result<InPlaceHeap> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: self.block.bytes(),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
        };
        // SAFETY: the section handle is valid while `self` lives, and the heap keeps the
        // section's memory alive after that. `desc` and the out-pointer are valid.
        unsafe {
            let heap: ID3D12Heap = device
                .cast::<ID3D12Device3>()?
                .OpenExistingHeapFromFileMapping(self.block.handle())?;
            let mut buffer: Option<ID3D12Resource> = None;
            device.CreatePlacedResource(
                &heap,
                0,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )?;
            Ok(InPlaceHeap {
                heap,
                buffer: buffer.expect("CreatePlacedResource succeeded without a resource"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{
        MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, PAGE_READWRITE, VirtualQuery,
    };

    const MIB: u64 = 1 << 20;

    fn memory() -> TitleMemory {
        TitleMemory::create(64 * MIB).unwrap()
    }

    /// What Windows says the protection of the page at `address` is.
    fn protection(address: u64) -> u32 {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `info` is a valid out-pointer of the size passed.
        let written = unsafe {
            VirtualQuery(
                Some(address as *const _),
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        assert_ne!(written, 0);
        info.Protect.0
    }

    fn bytes<'a>(address: u64, len: u64) -> &'a mut [u8] {
        // SAFETY: each test passes a range it has committed and keeps to itself.
        unsafe { std::slice::from_raw_parts_mut(address as *mut u8, len as usize) }
    }

    #[test]
    fn block_size_must_be_a_non_zero_multiple_of_64k() {
        assert_eq!(
            TitleMemory::create(0).err(),
            Some(MemError::InvalidParameter)
        );
        assert_eq!(
            TitleMemory::create(MIB + PAGE).err(),
            Some(MemError::InvalidParameter)
        );
    }

    #[test]
    fn address_conversion_is_one_subtraction() {
        let map = AddressMap {
            base: 0x7ff0_0000_0000,
            size: 14 << 30,
        };
        assert_eq!(map.offset(0x7ff0_0000_1234), 0x1234);
        assert_eq!(map.address(0x1234), 0x7ff0_0000_1234);
        assert_eq!(map.checked_offset(map.base), Some(0));
        assert_eq!(
            map.checked_offset(map.base + map.size - 1),
            Some(map.size - 1)
        );
        // Outside, below and above: the unchecked offset is out of range, the checked one refuses.
        assert_eq!(map.checked_offset(map.base + map.size), None);
        assert_eq!(map.checked_offset(map.base - 1), None);
        assert!(map.offset(map.base - 1) >= map.size);
    }

    #[test]
    fn title_addresses_are_host_addresses_inside_the_block() {
        let mem = memory();
        let map = mem.address_map();
        let a = mem.allocate(MIB).unwrap();
        assert_eq!(map.checked_offset(a), Some(0));
        bytes(a, 4).copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(bytes(map.address(2), 2), &[3, 4]);
    }

    #[test]
    fn pages_are_inaccessible_until_committed_and_again_after_decommit() {
        let mem = memory();
        let a = mem.reserve(None, MIB).unwrap();
        assert_eq!(protection(a), PAGE_NOACCESS.0);
        assert_eq!(mem.query(a).unwrap().state, State::Reserved);

        assert_eq!(mem.commit(a + PAGE + 100, 10), Ok(a + PAGE));
        assert_eq!(protection(a), PAGE_NOACCESS.0);
        assert_eq!(protection(a + PAGE), PAGE_READWRITE.0);
        assert_eq!(protection(a + 2 * PAGE), PAGE_NOACCESS.0);

        mem.decommit(a + PAGE, PAGE).unwrap();
        assert_eq!(protection(a + PAGE), PAGE_NOACCESS.0);
    }

    #[test]
    fn committed_pages_read_as_zero_also_after_a_decommit() {
        let mem = memory();
        let a = mem.allocate(4 * PAGE).unwrap();
        assert!(bytes(a, 4 * PAGE).iter().all(|b| *b == 0));
        bytes(a, 4 * PAGE).fill(0xAB);

        mem.decommit(a + PAGE, 2 * PAGE).unwrap();
        mem.commit(a, 4 * PAGE).unwrap();
        // The pages that stayed committed keep their bytes; the two that came back are zero.
        assert!(bytes(a, PAGE).iter().all(|b| *b == 0xAB));
        assert!(bytes(a + PAGE, 2 * PAGE).iter().all(|b| *b == 0));
        assert!(bytes(a + 3 * PAGE, PAGE).iter().all(|b| *b == 0xAB));
    }

    #[test]
    fn a_released_range_is_inaccessible_free_and_zero_when_used_again() {
        let mem = memory();
        let a = mem.allocate(MIB).unwrap();
        bytes(a, MIB).fill(0xCD);
        mem.release(a).unwrap();
        assert_eq!(protection(a), PAGE_NOACCESS.0);
        assert_eq!(mem.query(a).unwrap().state, State::Free);
        assert_eq!(mem.release(a), Err(MemError::InvalidAddress));

        let again = mem.allocate(MIB).unwrap();
        assert_eq!(again, a);
        assert!(bytes(again, MIB).iter().all(|b| *b == 0));
    }

    #[test]
    fn a_reservation_at_an_address_starts_there() {
        let mem = memory();
        let base = mem.address_map().base;
        assert_eq!(mem.reserve(Some(base + 8 * MIB), MIB), Ok(base + 8 * MIB));
        assert_eq!(
            mem.reserve(Some(base + 8 * MIB), MIB),
            Err(MemError::InvalidAddress)
        );
        assert_eq!(
            mem.reserve(Some(base - PAGE), MIB),
            Err(MemError::InvalidAddress)
        );
        assert_eq!(
            mem.reserve(Some(base + 64 * MIB), MIB),
            Err(MemError::InvalidAddress)
        );
    }

    #[test]
    fn query_reports_addresses() {
        // As on Windows, the run starts at the page asked about, not at the start of its state.
        let mem = memory();
        let a = mem.reserve(None, MIB).unwrap();
        mem.commit(a, 2 * PAGE).unwrap();
        assert_eq!(
            mem.query(a + PAGE),
            Ok(Region {
                base: a + PAGE,
                allocation_base: Some(a),
                size: PAGE,
                state: State::Committed
            })
        );
    }

    #[test]
    fn threads_allocate_side_by_side_without_sharing_a_range() {
        let mem = memory();
        let mut all: Vec<u64> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|t| {
                    let mem = &mem;
                    scope.spawn(move || {
                        (0..16)
                            .map(|_| {
                                let a = mem.allocate(GRANULARITY).unwrap();
                                bytes(a, GRANULARITY).fill(t);
                                a
                            })
                            .collect::<Vec<u64>>()
                    })
                })
                .collect();
            threads
                .into_iter()
                .flat_map(|t| t.join().unwrap())
                .collect()
        });
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 8 * 16);
        // Each range still holds one thread's byte throughout.
        for a in all {
            let range = bytes(a, GRANULARITY);
            assert!(range.iter().all(|b| *b == range[0]));
        }
    }
}
