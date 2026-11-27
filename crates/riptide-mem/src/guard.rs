//! Write guards: hearing about the first write to a resource (decisions D3 and D4).
//!
//! Promotion and texture conversion keep a copy of title data in video memory. The copy is
//! good for as long as the title does not write the original. A guard write-protects the
//! original's pages. The first write faults; the guard takes the protection off the whole
//! resource, notes the resource as written, and lets the write go on. Later writes cost
//! nothing.
//!
//! The fault is caught by a vectored exception handler, which Windows calls before any of
//! the title's own handlers. It answers only for write faults on guarded pages and passes
//! every other fault on untouched, so a title's own exception handling keeps working.

use std::ffi::c_void;
use std::ops::Range;
use std::sync::{Mutex, MutexGuard, Once};

use windows::Win32::System::Memory::{
    MEMORY_BASIC_INFORMATION, PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE, VirtualProtect,
    VirtualQuery,
};

use crate::pages::PAGE;

const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
/// The first parameter of an access violation when the access was a write.
const ACCESS_WRITE: usize = 1;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

/// `EXCEPTION_RECORD` for x64.
#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut c_void,
    parameter_count: u32,
    parameters: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut c_void,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AddVectoredExceptionHandler(
        first: u32,
        handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32,
    ) -> *mut c_void;
}

/// One guarded resource.
struct Guard {
    /// Whose title memory it lies in: that block's base address.
    owner: u64,
    /// The caller's name for the resource.
    resource: u64,
    /// The whole pages the resource touches, as addresses.
    pages: Range<u64>,
}

#[derive(Default)]
struct Registry {
    guards: Vec<Guard>,
    /// Every block that has ever had a guard, so a late fault in one can be told from a
    /// fault that is none of our business.
    blocks: Vec<Range<u64>>,
    /// Resources written since their owner last asked, as (owner, resource).
    written: Vec<(u64, u64)>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    guards: Vec::new(),
    blocks: Vec::new(),
    written: Vec::new(),
});

fn registry() -> MutexGuard<'static, Registry> {
    // Every change to the registry is a single step, so it is whole even after a panic.
    REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Sets the protection of whole pages. Failure is ignored: the pages were protected by this
/// module a moment ago, so the call can only fail if they are gone, and then nothing is
/// left to protect.
fn protect(pages: Range<u64>, protection: PAGE_PROTECTION_FLAGS) -> bool {
    if pages.is_empty() {
        return true;
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the range is whole pages of a title memory block, checked by the caller of
    // `guard` to be committed.
    unsafe {
        VirtualProtect(
            pages.start as *const c_void,
            (pages.end - pages.start) as usize,
            protection,
            &mut old,
        )
    }
    .is_ok()
}

/// The parts of `pages` that none of `others` covers.
fn uncovered(pages: Range<u64>, others: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut covering: Vec<&Range<u64>> = others
        .iter()
        .filter(|other| other.start < pages.end && pages.start < other.end)
        .collect();
    covering.sort_by_key(|other| other.start);
    let mut free = Vec::new();
    let mut at = pages.start;
    for other in covering {
        if other.start > at {
            free.push(at..other.start);
        }
        at = at.max(other.end);
    }
    if at < pages.end {
        free.push(at..pages.end);
    }
    free
}

impl Registry {
    /// Takes out every guard `gone` picks, makes writable again every page no remaining
    /// guard still needs protected, and returns the guards taken out.
    fn remove(&mut self, mut gone: impl FnMut(&Guard) -> bool) -> Vec<Guard> {
        let (removed, kept): (Vec<Guard>, Vec<Guard>) = std::mem::take(&mut self.guards)
            .into_iter()
            .partition(|g| gone(g));
        self.guards = kept;
        let still: Vec<Range<u64>> = self.guards.iter().map(|g| g.pages.clone()).collect();
        for guard in &removed {
            for pages in uncovered(guard.pages.clone(), &still) {
                protect(pages, PAGE_READWRITE);
            }
        }
        removed
    }
}

/// Whether Windows says the page at `address` is writable right now.
fn writable(address: u64) -> bool {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `info` is a valid out-pointer of the size passed; any address may be queried.
    let filled = unsafe {
        VirtualQuery(
            Some(address as *const c_void),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    filled != 0 && info.Protect == PAGE_READWRITE
}

/// Called by Windows for every exception in the process, before any other handler.
unsafe extern "system" fn on_exception(pointers: *mut ExceptionPointers) -> i32 {
    // SAFETY: Windows passes valid exception pointers to a vectored handler.
    let record = unsafe { &*(*pointers).record };
    if record.code != EXCEPTION_ACCESS_VIOLATION
        || record.parameter_count < 2
        || record.parameters[0] != ACCESS_WRITE
    {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let address = record.parameters[1] as u64;

    let mut registry = registry();
    // Pages are protected whole, so a write anywhere on a guarded page ends every guard
    // that touches that page: after it, none of them would hear of a write there.
    let hit = registry.remove(|guard| guard.pages.contains(&address));
    if !hit.is_empty() {
        registry
            .written
            .extend(hit.iter().map(|guard| (guard.owner, guard.resource)));
        return EXCEPTION_CONTINUE_EXECUTION;
    }

    // No guard covers the address. If another thread's write to the same resource ended
    // the guard a moment ago, this fault is a late one: the page is writable by now, and
    // the write only has to be tried again.
    let ours = registry.blocks.iter().any(|block| block.contains(&address));
    drop(registry);
    if ours && writable(address) {
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    EXCEPTION_CONTINUE_SEARCH
}

fn install_handler() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        // SAFETY: `on_exception` has the signature Windows calls and lives as long as the
        // process. It is put first, so it sees a guarded write before the title's handlers.
        unsafe { AddVectoredExceptionHandler(1, on_exception) };
    });
}

/// Write-protects the whole pages covering `range` and guards them for `resource`.
/// `block` is the title memory block the range lies in. Returns false if `resource` is
/// already guarded in that block, or Windows will not protect the pages.
pub(crate) fn guard(block: Range<u64>, resource: u64, range: Range<u64>) -> bool {
    install_handler();
    let pages = range.start / PAGE * PAGE..range.end.div_ceil(PAGE) * PAGE;
    let mut registry = registry();
    if registry
        .guards
        .iter()
        .any(|guard| guard.owner == block.start && guard.resource == resource)
    {
        return false;
    }
    if !registry.blocks.contains(&block) {
        registry.blocks.push(block.clone());
    }
    // The guard is in the registry before the pages are protected: a write that faults the
    // moment protection lands finds it there.
    registry.guards.push(Guard {
        owner: block.start,
        resource,
        pages: pages.clone(),
    });
    if protect(pages, PAGE_READONLY) {
        return true;
    }
    registry.remove(|guard| guard.owner == block.start && guard.resource == resource);
    false
}

/// Ends the guard for `resource` without a report. Returns whether there was one.
pub(crate) fn unguard(owner: u64, resource: u64) -> bool {
    !registry()
        .remove(|guard| guard.owner == owner && guard.resource == resource)
        .is_empty()
}

/// Ends every guard that touches `range`, reporting each as written: the pages are about to
/// be decommitted, so whatever was copied from them is no longer the title's data.
pub(crate) fn end_overlapping(owner: u64, range: Range<u64>) {
    let mut registry = registry();
    let ended = registry.remove(|guard| {
        guard.owner == owner && guard.pages.start < range.end && range.start < guard.pages.end
    });
    registry
        .written
        .extend(ended.iter().map(|guard| (guard.owner, guard.resource)));
}

/// The resources of `owner` written since the last call, in the order they were written.
pub(crate) fn take_written(owner: u64) -> Vec<u64> {
    let mut registry = registry();
    let mut taken = Vec::new();
    registry.written.retain(|&(whose, resource)| {
        if whose == owner {
            taken.push(resource);
        }
        whose != owner
    });
    taken
}

/// How many guards `owner` has.
pub(crate) fn count(owner: u64) -> usize {
    registry()
        .guards
        .iter()
        .filter(|guard| guard.owner == owner)
        .count()
}

/// Forgets a block that is being unmapped: its guards, its reports and its range.
pub(crate) fn forget(block: Range<u64>) {
    let mut registry = registry();
    // The pages are about to be unmapped, so their protection is left as it is.
    registry.guards.retain(|guard| guard.owner != block.start);
    registry.written.retain(|&(owner, _)| owner != block.start);
    registry.blocks.retain(|known| *known != block);
}

#[cfg(test)]
mod tests {
    use super::*;

    // These lists of ranges are meant as lists, also when they hold one.
    #[allow(clippy::single_range_in_vec_init)]
    #[test]
    fn pages_no_other_guard_covers_are_found() {
        assert_eq!(uncovered(0..10, &[]), [0..10]);
        assert_eq!(uncovered(0..10, &[20..30]), [0..10]);
        assert_eq!(uncovered(0..10, &[0..10]), []);
        assert_eq!(uncovered(0..10, &[3..5]), [0..3, 5..10]);
        assert_eq!(uncovered(0..10, &[8..20, 0..2]), [2..8]);
        assert_eq!(uncovered(0..10, &[2..6, 4..8]), [0..2, 8..10]);
        assert_eq!(uncovered(5..10, &[0..7, 9..12]), [7..9]);
    }
}
