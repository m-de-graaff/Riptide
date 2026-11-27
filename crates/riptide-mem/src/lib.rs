//! The title's memory: banks of RAM the GPU uses in place, the frames they are made of, the
//! title's addresses and what is mapped at them, write guards and the RAM budget.
//!
//! The design is `docs/design/2026-11-27-title-memory.md` (decisions M1 to M8). See
//! README.md in this folder for the interface.

mod budget;
mod frames;
mod guard;
mod os;
mod space;

pub use budget::{Pressure, RamBudget, RamUse, free_ram};
pub use frames::{BANK_BYTES, BANK_FRAMES, FRAME, FrameRun};
pub use guard::PAGE;
pub use os::Limits;
pub use space::Located;

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use frames::Frames;
use os::Section;
use space::Space;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
    D3D12_RESOURCE_FLAGS, D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
    ID3D12Device, ID3D12Device3, ID3D12Heap, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::Interface;

/// Why a memory call failed. `riptide-hle` turns these into the console's error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemError {
    /// The address is not in a reservation, or not in the state the call needs.
    InvalidAddress,
    /// A size or a frame number that cannot be right.
    InvalidParameter,
    /// No frames are left under the limit this title memory was made with.
    OutOfMemory,
    /// Windows refused, with this error code.
    Os(u32),
}

impl std::fmt::Display for MemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemError::InvalidAddress => write!(f, "the address is not usable for this call"),
            MemError::InvalidParameter => write!(f, "a size or frame number is not valid"),
            MemError::OutOfMemory => write!(f, "title memory is used up"),
            MemError::Os(code) => write!(f, "Windows refused with error {code}"),
        }
    }
}

impl std::error::Error for MemError {}

impl From<std::io::Error> for MemError {
    fn from(error: std::io::Error) -> Self {
        MemError::Os(error.raw_os_error().unwrap_or(0) as u32)
    }
}

/// How the CPU may reach mapped memory. The GPU reaches all of it regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
    None,
}

impl Access {
    fn protection(self) -> u32 {
        match self {
            Access::ReadWrite => os::PAGE_READWRITE,
            Access::ReadOnly => os::PAGE_READONLY,
            Access::None => os::PAGE_NOACCESS,
        }
    }
}

/// A bank opened for the GPU: a D3D12 heap over the bank's RAM and one buffer over all of
/// it. Offset `n` of the buffer is byte `n` of the bank.
pub struct InPlaceHeap {
    pub heap: ID3D12Heap,
    pub buffer: ID3D12Resource,
}

struct State {
    frames: Frames,
    /// One section per bank, in bank order.
    sections: Vec<Section>,
    space: Space,
    /// Frames that have been handed out before. A bank's frames start as zeros; one that has
    /// been used holds whatever it was left with, and is zeroed before it is handed out again.
    used: BTreeSet<u32>,
    /// Frames the title holds as physical pages. It frees them itself; unmapping does not.
    held: BTreeSet<u32>,
    /// Frames the title has freed that are still mapped somewhere. They go back when the
    /// last mapping does.
    leaving: BTreeSet<u32>,
}

/// All of a title's memory.
///
/// A title's addresses are host addresses: its code runs natively and uses them as pointers.
/// Every [`InPlaceHeap`] opened on it must be dropped before it is.
pub struct TitleMemory {
    id: u64,
    max_banks: usize,
    state: Mutex<State>,
    discard_failures: AtomicU64,
}

impl Drop for TitleMemory {
    fn drop(&mut self) {
        guard::forget(self.id);
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        while let Some(start) = state.space.first_reservation() {
            // A failure leaves address space reserved, which harms nothing.
            if state.space.release(start, &state.sections).is_err() {
                break;
            }
        }
    }
}

/// Groups frame numbers into runs of consecutive frames in one bank, in the order given.
fn runs_of(frames: &[u32]) -> Vec<FrameRun> {
    let mut runs: Vec<FrameRun> = Vec::new();
    for &frame in frames {
        match runs.last_mut() {
            Some(run)
                if run.first + run.count == frame
                    && frame / BANK_FRAMES == run.first / BANK_FRAMES =>
            {
                run.count += 1;
            }
            _ => runs.push(FrameRun {
                first: frame,
                count: 1,
            }),
        }
    }
    runs
}

impl TitleMemory {
    /// Title memory that may grow to `max_bytes` of frames, rounded up to whole banks. No
    /// bank is made until memory is first asked for.
    pub fn new(max_bytes: u64) -> TitleMemory {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        TitleMemory {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            max_banks: max_bytes.div_ceil(BANK_BYTES) as usize,
            state: Mutex::new(State {
                frames: Frames::new(),
                sections: Vec::new(),
                space: Space::new(),
                used: BTreeSet::new(),
                held: BTreeSet::new(),
                leaving: BTreeSet::new(),
            }),
            discard_failures: AtomicU64::new(0),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Each call changes the state in steps that leave it whole.
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Takes `count` frames, making banks as needed (decision M6).
    fn take(&self, state: &mut State, count: u32) -> Result<Vec<FrameRun>, MemError> {
        let max_banks = self.max_banks;
        let mut refused = None;
        let State {
            frames, sections, ..
        } = state;
        let runs = frames.allocate(count, |frames| {
            if frames.banks() >= max_banks {
                return false;
            }
            match Section::create(BANK_BYTES) {
                Ok(section) => {
                    sections.push(section);
                    frames.add_bank();
                    true
                }
                Err(error) => {
                    refused = Some(MemError::from(error));
                    false
                }
            }
        });
        let runs = runs.ok_or(refused.unwrap_or(MemError::OutOfMemory))?;
        for run in &runs {
            for frame in run.first..run.first + run.count {
                if !state.used.insert(frame) {
                    let one = FrameRun {
                        first: frame,
                        count: 1,
                    };
                    // SAFETY: the frame was free, so nothing but the anchor maps it.
                    unsafe { state.sections[one.bank()].zero(one.offset(), FRAME) };
                }
            }
        }
        Ok(runs)
    }

    /// Gives frames back: their RAM returns to Windows at once (decision M8).
    fn give_back(&self, state: &mut State, run: FrameRun) {
        // SAFETY: the caller found the run unmapped everywhere, so only the anchor holds it.
        let discarded = unsafe { state.sections[run.bank()].discard(run.offset(), run.bytes()) };
        if !discarded {
            self.discard_failures.fetch_add(1, Ordering::Relaxed);
        }
        state.frames.free(run);
    }

    /// Deals with frames that have just been unmapped at `range`: ends the guards there,
    /// and gives back every frame that is no longer mapped anywhere and that the title does
    /// not hold as a physical page.
    fn unmapped(&self, state: &mut State, range: Range<u64>, runs: Vec<FrameRun>) {
        guard::end_overlapping(self.id, range);
        for run in runs {
            for frame in run.first..run.first + run.count {
                let one = FrameRun {
                    first: frame,
                    count: 1,
                };
                if state.space.is_mapped(one) {
                    continue;
                }
                if state.leaving.remove(&frame) || !state.held.contains(&frame) {
                    self.give_back(state, one);
                }
            }
        }
    }

    /// Reserves `bytes` of addresses inside `limits`. Nothing is mapped there yet.
    pub fn reserve(&self, bytes: u64, limits: Limits) -> Result<u64, MemError> {
        self.state()
            .space
            .reserve(bytes.next_multiple_of(FRAME), limits)
    }

    /// Commits memory: takes frames and maps them. With `address`, at that address, which
    /// must lie in a reservation; without, in a new reservation inside `limits`. Returns
    /// where the memory starts. It reads as zero. Sizes are rounded up to whole frames.
    pub fn commit(
        &self,
        address: Option<u64>,
        bytes: u64,
        limits: Limits,
        access: Access,
    ) -> Result<u64, MemError> {
        if bytes == 0 {
            return Err(MemError::InvalidParameter);
        }
        let mut state = self.state();
        let (start, fresh) = match address {
            Some(address) => (address / FRAME * FRAME, false),
            None => (
                state.space.reserve(bytes.next_multiple_of(FRAME), limits)?,
                true,
            ),
        };
        let end = address.unwrap_or(start) + bytes;
        let count = u32::try_from((end.next_multiple_of(FRAME) - start) / FRAME)
            .map_err(|_| MemError::InvalidParameter)?;
        let runs = self.take(&mut state, count)?;
        let mut at = start;
        for (index, &run) in runs.iter().enumerate() {
            let State {
                space, sections, ..
            } = &mut *state;
            match space.map(at, run, sections, access.protection()) {
                Ok(displaced) => self.unmapped(&mut state, at..at + run.bytes(), displaced),
                Err(error) => {
                    // Undo: what was mapped so far, the frames not yet mapped, the addresses.
                    let State {
                        space, sections, ..
                    } = &mut *state;
                    let mapped = space
                        .unmap(start..at.max(start + FRAME), sections)
                        .unwrap_or_default();
                    self.unmapped(&mut state, start..at, mapped);
                    for &left in &runs[index..] {
                        state.frames.free(left);
                    }
                    if fresh {
                        let State {
                            space, sections, ..
                        } = &mut *state;
                        let _ = space.release(start, sections);
                    }
                    return Err(error);
                }
            }
            at += run.bytes();
        }
        Ok(start)
    }

    /// Takes away what is mapped in `bytes` at `address` (whole frames). Frames the title
    /// does not hold as physical pages go back to Windows.
    pub fn unmap(&self, address: u64, bytes: u64) -> Result<(), MemError> {
        let range = address / FRAME * FRAME..(address + bytes).next_multiple_of(FRAME);
        let mut state = self.state();
        let State {
            space, sections, ..
        } = &mut *state;
        let removed = space.unmap(range.clone(), sections)?;
        self.unmapped(&mut state, range, removed);
        Ok(())
    }

    /// Gives back the reservation that starts at `address`, and what is mapped in it.
    pub fn release(&self, address: u64) -> Result<(), MemError> {
        let mut state = self.state();
        let bytes = state
            .space
            .reservation(address)
            .ok_or(MemError::InvalidAddress)?;
        let State {
            space, sections, ..
        } = &mut *state;
        let removed = space.release(address, sections)?;
        self.unmapped(&mut state, address..address + bytes, removed);
        Ok(())
    }

    /// Hands the title `count` frames as physical pages, by number. They are mapped with
    /// [`TitleMemory::map_frames`] and stay the title's until [`TitleMemory::free_frames`].
    pub fn allocate_frames(&self, count: u32) -> Result<Vec<u32>, MemError> {
        let mut state = self.state();
        let runs = self.take(&mut state, count)?;
        let frames: Vec<u32> = runs
            .iter()
            .flat_map(|run| run.first..run.first + run.count)
            .collect();
        state.held.extend(&frames);
        Ok(frames)
    }

    /// Takes physical pages back. One that is still mapped stays readable there and goes
    /// back to Windows when its last mapping does.
    pub fn free_frames(&self, frames: &[u32]) -> Result<(), MemError> {
        let mut state = self.state();
        if frames.iter().any(|frame| !state.held.contains(frame)) {
            return Err(MemError::InvalidParameter);
        }
        for &frame in frames {
            state.held.remove(&frame);
            let one = FrameRun {
                first: frame,
                count: 1,
            };
            if state.space.is_mapped(one) {
                state.leaving.insert(frame);
            } else {
                self.give_back(&mut state, one);
            }
        }
        Ok(())
    }

    /// Maps physical pages at `address`, one after another, in place of what is mapped
    /// there. A frame may be mapped at several addresses at once.
    pub fn map_frames(&self, address: u64, frames: &[u32], access: Access) -> Result<(), MemError> {
        if !address.is_multiple_of(FRAME) {
            return Err(MemError::InvalidAddress);
        }
        let mut state = self.state();
        if frames.iter().any(|frame| !state.held.contains(frame)) {
            return Err(MemError::InvalidParameter);
        }
        let mut at = address;
        for run in runs_of(frames) {
            let State {
                space, sections, ..
            } = &mut *state;
            let displaced = space.map(at, run, sections, access.protection())?;
            self.unmapped(&mut state, at..at + run.bytes(), displaced);
            at += run.bytes();
        }
        Ok(())
    }

    /// Where in the banks `bytes` at `address` lie (decision M4), or `None` if any of it is
    /// not mapped. One part means the range can be used by the GPU where it is; several
    /// mean it has to be copied (decision M5).
    pub fn locate(&self, address: u64, bytes: u64) -> Option<Vec<Located>> {
        self.state().space.locate(address, bytes)
    }

    /// The whole pages of `bytes` at `address`, at every address their frames are mapped at.
    /// `None` if any of the range is not mapped.
    fn pages_everywhere(state: &State, address: u64, bytes: u64) -> Option<Vec<Range<u64>>> {
        let mut pages = Vec::new();
        for part in state.space.locate(address, bytes)? {
            let first = part.bank as u32 * BANK_FRAMES + (part.offset / FRAME) as u32;
            let last =
                part.bank as u32 * BANK_FRAMES + ((part.offset + part.bytes - 1) / FRAME) as u32;
            let run = FrameRun {
                first,
                count: last - first + 1,
            };
            for (view, view_first) in state.space.views_of(run) {
                // The part's bytes as addresses in this view.
                let view_offset = u64::from(view_first % BANK_FRAMES) * FRAME;
                let from = part.offset.max(view_offset);
                let to = (part.offset + part.bytes).min(view_offset + (view.end - view.start));
                if from < to {
                    let at = view.start + (from - view_offset);
                    pages.push(guard::pages_of(&(at..at + (to - from))));
                }
            }
        }
        Some(pages)
    }

    /// Says that `bytes` at `address` are about to be written by something a guard cannot
    /// hear: a system call, which fails on a protected page instead of faulting, or the
    /// GPU, which page protection does not bind. The guards there are ended and reported as
    /// written, as the write itself would have done. An address that is not title memory is
    /// nobody's to guard, and nothing happens.
    pub fn will_write(&self, address: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let state = self.state();
        if let Some(pages) = Self::pages_everywhere(&state, address, bytes) {
            for range in pages {
                guard::end_overlapping(self.id, range);
            }
        }
    }

    /// Copies `data` into physical pages the title holds, starting `offset` bytes into the
    /// first, whether or not they are mapped anywhere. Guards on them are ended and
    /// reported.
    pub fn write_frames(&self, frames: &[u32], offset: usize, data: &[u8]) -> Result<(), MemError> {
        let state = self.state();
        let room = frames.len() * FRAME as usize;
        if offset > room || data.len() > room - offset {
            return Err(MemError::InvalidParameter);
        }
        if frames.iter().any(|frame| !state.held.contains(frame)) {
            return Err(MemError::InvalidParameter);
        }
        let mut written = 0;
        let mut at = offset;
        while written < data.len() {
            let frame = frames[at / FRAME as usize];
            let within = at % FRAME as usize;
            let count = (FRAME as usize - within).min(data.len() - written);
            let one = FrameRun {
                first: frame,
                count: 1,
            };
            for (view, _) in state.space.views_of(one) {
                guard::end_overlapping(self.id, view);
            }
            // SAFETY: the frame lies inside its bank's anchor view, which is readable and
            // writable for the bank's life, and `within + count` does not pass its end.
            unsafe {
                let to = state.sections[one.bank()]
                    .anchor()
                    .add(one.offset() as usize + within);
                std::ptr::copy_nonoverlapping(data.as_ptr().add(written), to, count);
            }
            written += count;
            at += count;
        }
        Ok(())
    }

    /// Write-protects the pages of `bytes` at `address`, at every address their frames are
    /// mapped at, and guards them for `resource`, a number of the caller's choosing
    /// (decisions D3, D4 and M7).
    ///
    /// The first write to any of those pages, through any of those addresses, takes the
    /// protection off all of them, puts `resource` in the list [`TitleMemory::take_written`]
    /// returns, and then goes on as if nothing had happened. Reads are never disturbed.
    ///
    /// Pages are protected whole. A write beside the resource but on one of its pages ends
    /// the guard too: it is reported as written though it was not, which costs a copy and
    /// never a wrong picture. Guard a resource before copying it, not after.
    pub fn guard(&self, address: u64, bytes: u64, resource: u64) -> Result<(), MemError> {
        if bytes == 0 {
            return Err(MemError::InvalidParameter);
        }
        let state = self.state();
        let pages =
            Self::pages_everywhere(&state, address, bytes).ok_or(MemError::InvalidAddress)?;
        // Holding the state keeps the mappings in place until the guard stands.
        if guard::guard(self.id, resource, pages) {
            Ok(())
        } else {
            Err(MemError::InvalidParameter)
        }
    }

    /// Ends the guard for `resource` without reporting it as written. Returns whether it
    /// was guarded: false if a write, or an unmapping, ended the guard first.
    pub fn unguard(&self, resource: u64) -> bool {
        guard::unguard(self.id, resource)
    }

    /// The resources whose guard was ended by a write, or by their pages being unmapped,
    /// since the last call. Each appears once, in the order it first happened.
    pub fn take_written(&self) -> Vec<u64> {
        guard::take_written(self.id)
    }

    /// How many resources are guarded right now.
    pub fn guard_count(&self) -> usize {
        guard::count(self.id)
    }

    /// How many bytes of frames are handed out right now.
    pub fn committed_bytes(&self) -> u64 {
        self.state().frames.in_use() * FRAME
    }

    /// How many banks have been made.
    pub fn bank_count(&self) -> usize {
        self.state().sections.len()
    }

    /// How often Windows refused to give freed frames' RAM back. Their RAM stayed in use
    /// until they were handed out again.
    pub fn discard_failures(&self) -> u64 {
        self.discard_failures.load(Ordering::Relaxed)
    }

    /// The RAM this process uses now: title memory that is handed out, and everything else
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

    /// Opens bank `bank` for the GPU (decision M1): a heap on the bank's RAM, opened from
    /// its address, with one buffer over all of it.
    ///
    /// `flags` are added to `ALLOW_CROSS_ADAPTER`, which a buffer in such a heap must have.
    /// The GPU reads and writes every byte of the bank, mapped or not: page protection
    /// applies to the CPU only. The heap must be dropped before this title memory is.
    pub fn open_heap(
        &self,
        bank: usize,
        device: &ID3D12Device,
        flags: D3D12_RESOURCE_FLAGS,
    ) -> windows::core::Result<InPlaceHeap> {
        let anchor = self
            .state()
            .sections
            .get(bank)
            .map(Section::anchor)
            .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG))?;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: BANK_BYTES,
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
        // SAFETY: the anchor view stays mapped for as long as `self` lives, and the caller
        // drops the heap first. `desc` and the out-pointer are valid.
        unsafe {
            let heap: ID3D12Heap = device
                .cast::<ID3D12Device3>()?
                .OpenExistingHeapFromAddress(anchor.cast())?;
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

    const MIB: u64 = 1 << 20;
    const ANYWHERE: Limits = Limits {
        lowest: 0x1_0000_0000,
        highest: (1 << 40) - 1,
        alignment: 0,
    };

    fn write(address: u64, value: u8) {
        // SAFETY: every caller passes an address it has just mapped read-write.
        unsafe { (address as *mut u8).write_volatile(value) }
    }

    fn read(address: u64) -> u8 {
        // SAFETY: every caller passes an address it has just mapped.
        unsafe { (address as *const u8).read_volatile() }
    }

    #[test]
    fn frame_numbers_are_grouped_into_runs() {
        assert_eq!(
            runs_of(&[5, 6, 7, 9, 10, 3]),
            [
                FrameRun { first: 5, count: 3 },
                FrameRun { first: 9, count: 2 },
                FrameRun { first: 3, count: 1 }
            ]
        );
        // Consecutive numbers across two banks are two runs.
        assert_eq!(runs_of(&[BANK_FRAMES - 1, BANK_FRAMES]).len(), 2);
    }

    #[test]
    fn committed_memory_is_usable_inside_its_limits_and_counted() {
        let memory = TitleMemory::new(4 * BANK_BYTES);
        let low = Limits {
            lowest: 0x1_0000,
            highest: u64::from(u32::MAX),
            alignment: 0,
        };
        let at = memory
            .commit(None, 3 * MIB, low, Access::ReadWrite)
            .unwrap();
        assert!(at + 3 * MIB - 1 <= u64::from(u32::MAX));
        assert_eq!(read(at), 0);
        write(at, 1);
        write(at + 3 * MIB - 1, 2);
        assert_eq!((read(at), read(at + 3 * MIB - 1)), (1, 2));
        assert_eq!(memory.committed_bytes(), 3 * MIB);
        assert_eq!(memory.bank_count(), 1);

        // One part: the GPU could use it where it is.
        let parts = memory.locate(at, 3 * MIB).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(
            (parts[0].address, parts[0].bank, parts[0].bytes),
            (at, 0, 3 * MIB)
        );

        memory.release(at).unwrap();
        assert_eq!(memory.committed_bytes(), 0);
        assert_eq!(memory.locate(at, 1), None);
    }

    #[test]
    fn an_alignment_is_kept() {
        let memory = TitleMemory::new(BANK_BYTES);
        let aligned = Limits {
            alignment: 2 * MIB,
            ..ANYWHERE
        };
        let at = memory
            .commit(None, 4 * MIB, aligned, Access::ReadWrite)
            .unwrap();
        assert_eq!(at % (2 * MIB), 0);
        assert!(at >= ANYWHERE.lowest);
    }

    #[test]
    fn physical_pages_map_where_the_title_says_and_at_two_addresses_at_once() {
        let memory = TitleMemory::new(BANK_BYTES);
        let range = memory.reserve(64 * FRAME, ANYWHERE).unwrap();
        let frames = memory.allocate_frames(4).unwrap();
        assert_eq!(memory.locate(range, FRAME), None);

        memory
            .map_frames(range + 8 * FRAME, &frames, Access::ReadWrite)
            .unwrap();
        // The same pages again, elsewhere and in another order.
        let again = [frames[2], frames[0]];
        memory
            .map_frames(range + 32 * FRAME, &again, Access::ReadWrite)
            .unwrap();

        write(range + 8 * FRAME + 5, 0xA5);
        assert_eq!(read(range + 33 * FRAME + 5), 0xA5);
        write(range + 32 * FRAME + 9, 0x5A);
        assert_eq!(read(range + 10 * FRAME + 9), 0x5A);

        // Four frames in a row are one part; the two out of order are two.
        assert_eq!(
            memory.locate(range + 8 * FRAME, 4 * FRAME).unwrap().len(),
            1
        );
        assert_eq!(
            memory.locate(range + 32 * FRAME, 2 * FRAME).unwrap().len(),
            2
        );
        // A range that runs into unmapped addresses is not located at all.
        assert_eq!(memory.locate(range + 8 * FRAME, 5 * FRAME), None);
    }

    #[test]
    fn unmapping_the_middle_keeps_the_rest_and_the_pages_keep_their_bytes() {
        let memory = TitleMemory::new(BANK_BYTES);
        let range = memory.reserve(16 * FRAME, ANYWHERE).unwrap();
        let frames = memory.allocate_frames(6).unwrap();
        memory
            .map_frames(range, &frames, Access::ReadWrite)
            .unwrap();
        for index in 0..6 {
            write(range + index * FRAME, index as u8 + 1);
        }
        memory.unmap(range + 2 * FRAME, 2 * FRAME).unwrap();
        assert_eq!((read(range), read(range + FRAME)), (1, 2));
        assert_eq!((read(range + 4 * FRAME), read(range + 5 * FRAME)), (5, 6));
        assert_eq!(memory.locate(range + 2 * FRAME, FRAME), None);

        // The title still holds the pages, so mapping them again shows what they held.
        memory
            .map_frames(range + 10 * FRAME, &frames[2..4], Access::ReadWrite)
            .unwrap();
        assert_eq!((read(range + 10 * FRAME), read(range + 11 * FRAME)), (3, 4));
        assert_eq!(memory.committed_bytes(), 6 * FRAME);
    }

    #[test]
    fn freed_pages_go_back_when_their_last_mapping_does_and_read_as_zero_next_time() {
        let memory = TitleMemory::new(BANK_BYTES);
        let range = memory.reserve(8 * FRAME, ANYWHERE).unwrap();
        let frames = memory.allocate_frames(2).unwrap();
        memory
            .map_frames(range, &frames, Access::ReadWrite)
            .unwrap();
        write(range, 9);

        memory.free_frames(&frames).unwrap();
        // Still mapped, so still there and still counted.
        assert_eq!(read(range), 9);
        assert_eq!(memory.committed_bytes(), 2 * FRAME);
        memory.unmap(range, 2 * FRAME).unwrap();
        assert_eq!(memory.committed_bytes(), 0);
        assert_eq!(memory.discard_failures(), 0);

        // The same frames come out again, empty.
        let next = memory.allocate_frames(2).unwrap();
        assert_eq!(next, frames);
        memory.map_frames(range, &next, Access::ReadWrite).unwrap();
        assert_eq!(read(range), 0);
        // A page the title does not hold cannot be freed or mapped.
        assert_eq!(
            memory.free_frames(&[12345]),
            Err(MemError::InvalidParameter)
        );
        assert_eq!(
            memory.map_frames(range, &[12345], Access::ReadWrite),
            Err(MemError::InvalidParameter)
        );
    }

    #[test]
    fn committed_memory_gives_its_frames_back_when_unmapped() {
        let memory = TitleMemory::new(BANK_BYTES);
        let at = memory
            .commit(None, 8 * FRAME, ANYWHERE, Access::ReadWrite)
            .unwrap();
        write(at + 3 * FRAME, 7);
        memory.unmap(at + 2 * FRAME, 4 * FRAME).unwrap();
        assert_eq!(memory.committed_bytes(), 4 * FRAME);
        // Committing there again gives zeroed memory.
        memory
            .commit(Some(at + 2 * FRAME), 4 * FRAME, ANYWHERE, Access::ReadWrite)
            .unwrap();
        assert_eq!(read(at + 3 * FRAME), 0);
        assert_eq!(memory.committed_bytes(), 8 * FRAME);
    }

    #[test]
    fn the_limit_is_kept() {
        let memory = TitleMemory::new(BANK_BYTES);
        assert!(memory.allocate_frames(BANK_FRAMES - 1).is_ok());
        assert_eq!(memory.allocate_frames(1), Err(MemError::OutOfMemory));
        assert_eq!(memory.bank_count(), 1);
    }
}
