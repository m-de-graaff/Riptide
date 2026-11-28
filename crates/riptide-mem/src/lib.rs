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
    /// The ranges of title addresses whose mapping has changed since they were last asked
    /// for, for whoever keeps a copy of the page table.
    remapped: Vec<Range<u64>>,
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
/// Adds `range` to a list of remapped ranges, joined to the last one if it follows on.
/// A list that has grown long is made one range that covers everything in it: whoever
/// reads it then looks at more than changed, which costs time and is never wrong.
fn note_remapped(remapped: &mut Vec<Range<u64>>, range: Range<u64>) {
    if range.start >= range.end {
        return;
    }
    match remapped.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => remapped.push(range),
    }
    if remapped.len() > 4096 {
        let start = remapped.iter().map(|range| range.start).min().unwrap_or(0);
        let end = remapped.iter().map(|range| range.end).max().unwrap_or(0);
        remapped.clear();
        remapped.push(start..end);
    }
}

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
                remapped: Vec::new(),
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
        note_remapped(&mut state.remapped, range.clone());
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

    /// Gives back the part that starts at `address`, and what is mapped in it: a whole
    /// reservation, or one of the parts the title cut it into with [`TitleMemory::cut`].
    pub fn release(&self, address: u64) -> Result<(), MemError> {
        let mut state = self.state();
        let part = state.space.part(address).ok_or(MemError::InvalidAddress)?;
        let State {
            space, sections, ..
        } = &mut *state;
        let removed = space.release(address, sections)?;
        self.unmapped(&mut state, part, removed);
        Ok(())
    }

    /// Whether `address` lies in a reservation of this title memory.
    pub fn holds(&self, address: u64) -> bool {
        self.state().space.holds(address)
    }

    /// The size of the part that starts exactly at `address`: a whole reservation, or what
    /// lies between two cuts in one.
    pub fn part(&self, address: u64) -> Option<u64> {
        let part = self.state().space.part(address)?;
        Some(part.end - part.start)
    }

    /// Makes `bytes` at `address` a part of its own in its reservation, as a title does
    /// before it fills the parts of a reservation separately. What is mapped there is taken
    /// away. Whole frames only.
    pub fn cut(&self, address: u64, bytes: u64) -> Result<(), MemError> {
        let range = address
            ..address
                .checked_add(bytes)
                .ok_or(MemError::InvalidParameter)?;
        let mut state = self.state();
        let State {
            space, sections, ..
        } = &mut *state;
        let removed = space.cut(range.clone(), sections)?;
        self.unmapped(&mut state, range, removed);
        Ok(())
    }

    /// Undoes the cuts strictly inside `bytes` at `address`. Nothing mapped changes.
    pub fn uncut(&self, address: u64, bytes: u64) -> Result<(), MemError> {
        let range = address
            ..address
                .checked_add(bytes)
                .ok_or(MemError::InvalidParameter)?;
        self.state().space.uncut(range)
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

    /// Maps at `to` the same memory that is mapped in `bytes` at `from`, in place of what is
    /// mapped at `to`: afterwards both addresses reach the same frames. This is how a title
    /// gives pages of a pool it filled to a texture's addresses. Whole frames only, and all
    /// of the source must be mapped.
    pub fn alias(&self, to: u64, from: u64, bytes: u64) -> Result<(), MemError> {
        if !to.is_multiple_of(FRAME) || !from.is_multiple_of(FRAME) || !bytes.is_multiple_of(FRAME)
        {
            return Err(MemError::InvalidAddress);
        }
        let mut state = self.state();
        let parts = state
            .space
            .locate(from, bytes)
            .ok_or(MemError::InvalidAddress)?;
        let mut at = to;
        for part in parts {
            let run = FrameRun {
                first: (part.bank as u64 * u64::from(BANK_FRAMES) + part.offset / FRAME) as u32,
                count: (part.bytes / FRAME) as u32,
            };
            let State {
                space, sections, ..
            } = &mut *state;
            let displaced = space.map(at, run, sections, os::PAGE_READWRITE)?;
            self.unmapped(&mut state, at..at + part.bytes, displaced);
            at += part.bytes;
        }
        Ok(())
    }

    /// The ranges of title addresses whose mapping has changed since this was last called:
    /// mapped, unmapped or mapped to other frames. For keeping a copy of the page table
    /// (the GPU's) up to date without reading all of it again.
    pub fn take_remapped(&self) -> Vec<Range<u64>> {
        std::mem::take(&mut self.state().remapped)
    }

    /// What is mapped in `range`: each stretch of addresses that has frames one after
    /// another behind it, as its first address, its first frame and how many frames.
    pub fn mapped_in(&self, range: Range<u64>) -> Vec<(u64, u32, u32)> {
        self.state().space.mapped_in(range)
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

    /// As [`TitleMemory::guard`], for a range that need not be mapped whole: the pages of
    /// it that are mapped are guarded, and the rest left alone. A title maps a texture it
    /// streams a piece at a time. Returns how many bytes of the range are mapped, which is
    /// an error if none are.
    pub fn guard_mapped(&self, address: u64, bytes: u64, resource: u64) -> Result<u64, MemError> {
        if bytes == 0 {
            return Err(MemError::InvalidParameter);
        }
        let state = self.state();
        let mut pages = Vec::new();
        let mut mapped = 0;
        for (from, to) in Self::mapped_stretches(&state, address, bytes) {
            if let Some(more) = Self::pages_everywhere(&state, from, to - from) {
                pages.extend(more);
                mapped += to - from;
            }
        }
        if mapped == 0 {
            return Err(MemError::InvalidAddress);
        }
        // Holding the state keeps the mappings in place until the guard stands.
        if guard::guard(self.id, resource, pages) {
            Ok(mapped)
        } else {
            Err(MemError::InvalidParameter)
        }
    }

    /// The stretches of `bytes` at `address` that are mapped, each as its first address
    /// and the address after its last.
    fn mapped_stretches(state: &State, address: u64, bytes: u64) -> Vec<(u64, u64)> {
        let end = address.saturating_add(bytes);
        state
            .space
            .mapped_in(address..end)
            .into_iter()
            .map(|(first, _, frames)| {
                (
                    first.max(address),
                    (first + u64::from(frames) * FRAME).min(end),
                )
            })
            .filter(|(from, to)| from < to)
            .collect()
    }

    /// Copies what is mapped of the title memory at `address` into `into`, which stands
    /// for the bytes from that address on, and leaves the rest of `into` as it is. Returns
    /// how many bytes were copied. The title may be writing them meanwhile: what is read
    /// then is some mix of old and new.
    pub fn read_mapped(&self, address: u64, into: &mut [u8]) -> u64 {
        let state = self.state();
        let mut copied = 0;
        for (from, to) in Self::mapped_stretches(&state, address, into.len() as u64) {
            let count = (to - from) as usize;
            let at = (from - address) as usize;
            // SAFETY: the stretch is mapped, and stays so while the state is held; the
            // destination is that many bytes of `into`.
            unsafe {
                std::ptr::copy_nonoverlapping(from as *const u8, into[at..].as_mut_ptr(), count);
            }
            copied += count as u64;
        }
        copied
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

    /// What a title does with one reservation for several kinds of graphics memory: cuts
    /// it into parts, fills each with physical pages, and gives the parts back one by one.
    #[test]
    fn a_reservation_cut_into_parts_is_filled_and_given_back_part_by_part() {
        let memory = TitleMemory::new(BANK_BYTES);
        let base = memory.reserve(64 * FRAME, ANYWHERE).unwrap();
        let parts = [(0, 10), (10, 1), (11, 27), (38, 26)];
        for (first, count) in parts {
            memory.cut(base + first * FRAME, count * FRAME).unwrap();
        }
        for (first, count) in parts {
            assert_eq!(memory.part(base + first * FRAME), Some(count * FRAME));
        }
        assert_eq!(memory.part(base + 5 * FRAME), None);

        let frames = memory.allocate_frames(27).unwrap();
        memory
            .map_frames(base + 11 * FRAME, &frames, Access::ReadWrite)
            .unwrap();
        write(base + 11 * FRAME, 7);
        write(base + 37 * FRAME, 9);
        assert_eq!((read(base + 11 * FRAME), read(base + 37 * FRAME)), (7, 9));

        // Emptying a part leaves it a part: its neighbours are not joined to it.
        memory.unmap(base + 11 * FRAME, 27 * FRAME).unwrap();
        assert_eq!(memory.part(base + 11 * FRAME), Some(27 * FRAME));
        memory
            .map_frames(base + 11 * FRAME, &frames, Access::ReadWrite)
            .unwrap();

        // A middle part goes; the parts on either side stay, and each still goes alone.
        memory.release(base + 11 * FRAME).unwrap();
        assert!(!memory.holds(base + 11 * FRAME));
        assert_eq!(
            memory.release(base + 11 * FRAME),
            Err(MemError::InvalidAddress)
        );
        assert_eq!(memory.part(base), Some(10 * FRAME));
        assert_eq!(memory.part(base + 10 * FRAME), Some(FRAME));
        memory.release(base + 10 * FRAME).unwrap();
        memory.release(base).unwrap();
        memory.release(base + 38 * FRAME).unwrap();
        assert!(!memory.holds(base) && !memory.holds(base + 63 * FRAME));
        memory.free_frames(&frames).unwrap();
    }

    #[test]
    fn pages_mapped_across_a_cut_lie_on_both_sides_of_it() {
        let memory = TitleMemory::new(BANK_BYTES);
        let base = memory.reserve(16 * FRAME, ANYWHERE).unwrap();
        memory.cut(base + 4 * FRAME, 4 * FRAME).unwrap();
        let at = memory
            .commit(
                Some(base + 2 * FRAME),
                8 * FRAME,
                ANYWHERE,
                Access::ReadWrite,
            )
            .unwrap();
        assert_eq!(at, base + 2 * FRAME);
        for frame in 2..10 {
            write(base + frame * FRAME, frame as u8);
        }
        assert_eq!(read(base + 9 * FRAME), 9);
        assert!(memory.locate(base + 2 * FRAME, 8 * FRAME).is_some());

        // Cutting through mapped memory takes away what lies in the new part only.
        memory.cut(base + 8 * FRAME, FRAME).unwrap();
        assert_eq!(memory.locate(base + 8 * FRAME, FRAME), None);
        assert_eq!((read(base + 7 * FRAME), read(base + 9 * FRAME)), (7, 9));
        memory.release(base).unwrap();
    }

    #[test]
    fn memory_aliased_at_a_second_address_is_the_same_memory() {
        let memory = TitleMemory::new(BANK_BYTES);
        let pool = memory
            .commit(None, 8 * FRAME, ANYWHERE, Access::ReadWrite)
            .unwrap();
        let texture = memory.reserve(16 * FRAME, ANYWHERE).unwrap();
        write(pool + 3 * FRAME + 9, 0x5A);

        // Two pool pages, from the fourth on, at the texture's sixth page.
        memory
            .alias(texture + 5 * FRAME, pool + 3 * FRAME, 2 * FRAME)
            .unwrap();
        assert_eq!(read(texture + 5 * FRAME + 9), 0x5A);
        write(texture + 6 * FRAME, 0x33);
        assert_eq!(read(pool + 4 * FRAME), 0x33);
        assert_eq!(
            memory.locate(texture + 5 * FRAME, FRAME),
            memory.locate(pool + 3 * FRAME, FRAME).map(|mut parts| {
                parts[0].address = texture + 5 * FRAME;
                parts
            })
        );

        // The same page at every one of three addresses.
        for page in 0..3 {
            memory.alias(texture + page * FRAME, pool, FRAME).unwrap();
        }
        write(pool + 1, 0x77);
        assert_eq!(read(texture + 2 * FRAME + 1), 0x77);

        // Taking the alias away leaves the pool as it was.
        memory.unmap(texture, 16 * FRAME).unwrap();
        assert_eq!(read(pool + 3 * FRAME + 9), 0x5A);
        // What is not mapped cannot be aliased, nor can part of a frame.
        assert_eq!(
            memory.alias(texture, texture + 8 * FRAME, FRAME),
            Err(MemError::InvalidAddress)
        );
        assert_eq!(
            memory.alias(texture, pool + 1, FRAME),
            Err(MemError::InvalidAddress)
        );
        memory.release(texture).unwrap();
        memory.release(pool).unwrap();
    }

    #[test]
    fn what_changed_is_told_once_and_what_is_mapped_can_be_read_back() {
        let memory = TitleMemory::new(BANK_BYTES);
        let range = memory.reserve(16 * FRAME, ANYWHERE).unwrap();
        // Reserving maps nothing.
        assert!(memory.take_remapped().is_empty());
        let frames = memory.allocate_frames(3).unwrap();
        memory
            .map_frames(range + 4 * FRAME, &frames, Access::ReadWrite)
            .unwrap();
        let changed = memory.take_remapped();
        assert!(
            changed
                .iter()
                .any(|part| part.start <= range + 4 * FRAME && part.end >= range + 7 * FRAME)
        );
        assert!(memory.take_remapped().is_empty());

        let mapped = memory.mapped_in(range..range + 16 * FRAME);
        let total: u32 = mapped.iter().map(|&(_, _, count)| count).sum();
        assert_eq!(total, 3);
        assert_eq!(mapped[0].0, range + 4 * FRAME);
        assert_eq!(mapped[0].1, frames[0]);
        // A range that cuts through what is mapped gets the part inside it.
        let inside = memory.mapped_in(range + 5 * FRAME..range + 6 * FRAME);
        assert_eq!(inside.len(), 1);
        assert_eq!((inside[0].0, inside[0].2), (range + 5 * FRAME, 1));

        memory.unmap(range + 4 * FRAME, 3 * FRAME).unwrap();
        assert!(!memory.take_remapped().is_empty());
        assert!(memory.mapped_in(range..range + 16 * FRAME).is_empty());
        memory.release(range).unwrap();
        memory.free_frames(&frames).unwrap();
    }

    #[test]
    fn remapped_ranges_join_and_a_long_list_becomes_one_range() {
        let mut list = Vec::new();
        note_remapped(&mut list, 0..10);
        note_remapped(&mut list, 10..20);
        note_remapped(&mut list, 40..50);
        note_remapped(&mut list, 60..60);
        assert_eq!(list, [0..20, 40..50]);
        for index in 0..5000u64 {
            note_remapped(&mut list, index * 100 + 1000..index * 100 + 1001);
        }
        assert!(list.len() <= 4096);
        assert_eq!(list.iter().map(|range| range.start).min(), Some(0));
    }

    #[test]
    fn parts_joined_again_are_one_part() {
        let memory = TitleMemory::new(BANK_BYTES);
        let base = memory.reserve(8 * FRAME, ANYWHERE).unwrap();
        memory.cut(base + 2 * FRAME, 2 * FRAME).unwrap();
        assert_eq!(memory.part(base), Some(2 * FRAME));
        memory.uncut(base, 8 * FRAME).unwrap();
        assert_eq!(memory.part(base), Some(8 * FRAME));
        assert_eq!(memory.part(base + 2 * FRAME), None);
        // One placeholder again: Windows gives all of it back at once.
        memory.release(base).unwrap();
        assert!(!memory.holds(base));
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
