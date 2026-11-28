//! The console's memory functions: `xmem.dll`, and the one graphics call that sets up
//! addresses before any of them, `D3D12ConfigureVirtualMemoryX`.
//!
//! On the console the CPU and the GPU share one memory and one set of addresses, so a
//! graphics address is the title's own pointer. Everything these calls hand out is title
//! memory (`riptide-mem`): frames of banks the GPU can use where they are. This module keeps
//! only the console's rules about flags: page sizes, how many address bits memory may use,
//! and the title's own allocation hooks.

use std::ffi::c_void;
use std::sync::OnceLock;

use riptide_mem::{Access, FRAME, Limits, TitleMemory};
use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::sys;
use crate::trace::trace;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const PAGE_READWRITE: u32 = 0x04;

/// Allocation-type bit: 2 MiB pages. Windows reads this bit as large pages.
const PAGES_2M: u32 = 0x2000_0000;
/// Allocation-type bit that, with [`PAGES_2M`], means 64 KiB pages.
const PAGES_64K_BIT: u32 = 0x40_0000;
/// Windows' protection values. The console's bits above them say how the GPU reaches the
/// memory.
const HOST_PROTECTION: u32 = 0xFF;

/// `XMemVirtualAlloc` flag: the memory must lie below 4 GiB.
const FLAG_32BIT: u64 = 1 << 2;
/// `XMemAlloc` attributes: the alignment as a power of two, in five bits from bit 24.
const ATTRIBUTE_ALIGNMENT_SHIFT: u64 = 24;
const ATTRIBUTE_MEMORY_TYPE_SHIFT: u64 = 29;
/// How many graphics allocations of the default allocator have been written to the log.
static GRAPHICS_SHOWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// `XMemAlloc` attribute: the memory must lie below 4 GiB.
const ATTRIBUTE_32BIT: u64 = 1 << 33;

/// The console's page numbers count 4 KiB; a frame is 64 KiB, so a frame's page number is
/// its number times 16.
const PAGES_PER_FRAME: usize = (FRAME / 4096) as usize;

/// How much title memory there may be: the console gives a title about 13.5 GiB.
const TITLE_MEMORY_BYTES: u64 = 16 * GIB;

/// The title's memory. There is one title in a process.
pub fn memory() -> &'static TitleMemory {
    static MEMORY: OnceLock<TitleMemory> = OnceLock::new();
    MEMORY.get_or_init(|| TitleMemory::new(TITLE_MEMORY_BYTES))
}

/// What `D3D12ConfigureVirtualMemoryX` takes and fills in.
#[repr(C)]
pub struct VirtualMemoryConfiguration {
    flags: u32,
    /// The size of the GPU's page table, in 4 MiB pages.
    page_table_pages: u32,
    /// Memory kept across the title's processes for the frame buffer.
    preserved_bytes: u32,
    /// Out: the lowest and highest address graphics memory can have.
    lowest: u64,
    highest: u64,
}

const _: () = assert!(size_of::<VirtualMemoryConfiguration>() == 0x20);

/// `d3d12_xs.dll!D3D12ConfigureVirtualMemoryX`: called once, before any graphics memory is
/// asked for. Graphics addresses are the title's own addresses, so the range given back is
/// all the addresses a program can have.
///
/// # Safety
/// `configuration` is null or points at the title's configuration.
pub unsafe extern "system" fn configure_virtual_memory(
    configuration: *mut VirtualMemoryConfiguration,
) -> i32 {
    // SAFETY: the title passes its own structure; null is refused.
    let Some(configuration) = (unsafe { configuration.as_mut() }) else {
        return E_INVALIDARG;
    };
    let mut info = sys::SYSTEM_INFO::default();
    // SAFETY: `info` is a valid out-pointer.
    unsafe { sys::GetSystemInfo(&mut info) };
    configuration.lowest = info.minimum_application_address as u64;
    configuration.highest = info.maximum_application_address as u64;
    trace!(
        "D3D12ConfigureVirtualMemoryX: flags {:#x}, page table {} x 4 MiB, preserved {:#x} \
         bytes; graphics addresses {:#x} to {:#x}",
        configuration.flags,
        configuration.page_table_pages,
        configuration.preserved_bytes,
        configuration.lowest,
        configuration.highest
    );
    S_OK
}

/// Where memory with these `flags` may lie. Memory marked 32-bit lies below 4 GiB. Other
/// memory lies above it, so the low 4 GiB stay free for the 32-bit requests, and below a
/// limit the three bits from bit 12 give: 46 bits for 1 down to 40 for 7, and 40 with no
/// field. The title computes with these limits (its ray-tracing structures must lie below
/// 2^40). Whatever the field allows, nothing is given above 2^37: that is as far as the
/// page table the GPU reads title memory through goes, and it is inside every limit.
fn limits(flags: u64, alignment: u64) -> Limits {
    if flags & FLAG_32BIT != 0 {
        return Limits {
            lowest: 0x1_0000,
            highest: u64::from(u32::MAX),
            alignment,
        };
    }
    let bits = match (flags >> 12) & 7 {
        0 => 40,
        field => 47 - field,
    };
    // Never above what the GPU's page table covers (2^37), whatever more the title allows:
    // shaders read title memory through that table.
    Limits {
        lowest: 4 * GIB,
        highest: (1u64 << bits.min(37)) - 1,
        alignment,
    }
}

/// How the CPU may reach memory asked for with `protect`.
fn access(protect: u32) -> Access {
    match protect & HOST_PROTECTION {
        0 | 0x01 => Access::None,
        0x02 | 0x20 => Access::ReadOnly,
        _ => Access::ReadWrite,
    }
}

/// `xmem.dll!XMemVirtualAlloc`: `VirtualAlloc` with the console's page sizes in
/// `allocation_type`, its address limits in `flags`, and its GPU access bits in `protect`.
///
/// # Safety
/// The arguments are the title's own; `base` is null or an address in a reservation it made.
pub unsafe extern "system" fn virtual_alloc(
    base: *mut c_void,
    size: usize,
    allocation_type: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let pages_2m = allocation_type & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M;
    let commit = allocation_type & MEM_COMMIT != 0;
    let limits = limits(flags, if pages_2m { 2 * MIB } else { 0 });
    let result = if commit {
        // Frames are taken and mapped: at the address given, or in a new reservation.
        let at = (!base.is_null()).then_some(base as u64);
        memory().commit(at, size as u64, limits, access(protect))
    } else if !base.is_null() {
        if allocation_type & MEM_REPLACE_PLACEHOLDER != 0 {
            // The title takes a part it cut out of a reservation it already has, with
            // `VirtualFree`. The part stays empty address space until it is filled.
            if memory().part(base as u64) == Some(size as u64) {
                Ok(base as u64)
            } else {
                Err(riptide_mem::MemError::InvalidAddress)
            }
        } else {
            // A reservation at an address of the title's choosing has not been needed.
            Err(riptide_mem::MemError::InvalidParameter)
        }
    } else {
        // Address space only. Physical pages or a later commit fill it.
        memory().reserve(size as u64, limits)
    };
    let address = result.unwrap_or(0) as *mut c_void;
    trace!(
        "XMemVirtualAlloc(base {base:p}, size {size:#x}, type {allocation_type:#x}, flags \
         {flags:#x}, protect {protect:#x}) = {address:p}{}",
        match result {
            Ok(_) => String::new(),
            Err(error) => format!(": {error}"),
        }
    );
    if result.is_err() {
        // SAFETY: the call has no preconditions.
        unsafe { sys::SetLastError(ERROR_NOT_ENOUGH_MEMORY) };
    }
    address
}

const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INVALID_ADDRESS: u32 = 487;

const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
const MEM_PRESERVE_PLACEHOLDER: u32 = 0x2;

/// What a `VirtualFree` of title memory does, by its `free_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freeing {
    /// Takes the memory away and keeps the addresses.
    Decommit,
    /// Gives the addresses back, with what is mapped there.
    Release,
    /// Makes the range a part of its own in its reservation.
    Cut,
    /// Makes the parts in the range one part again.
    Join,
}

fn freeing(free_type: u32) -> Option<Freeing> {
    const CUT: u32 = MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER;
    const JOIN: u32 = MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS;
    match free_type {
        MEM_DECOMMIT => Some(Freeing::Decommit),
        MEM_RELEASE => Some(Freeing::Release),
        CUT => Some(Freeing::Cut),
        JOIN => Some(Freeing::Join),
        _ => None,
    }
}

/// `kernel32.dll!VirtualFree`. A title frees what `XMemVirtualAlloc` gave it with this, and
/// cuts a reservation into parts with it. Title memory is ours to keep the books of, so
/// those calls are done here; any other address goes to Windows.
///
/// # Safety
/// The arguments are the title's own.
pub unsafe extern "system" fn virtual_free(
    address: *mut c_void,
    size: usize,
    free_type: u32,
) -> i32 {
    let at = address as u64;
    if !memory().holds(at) {
        // SAFETY: not title memory: the title's own call, passed on unchanged.
        return unsafe { sys::VirtualFree(address, size, free_type) };
    }
    let bytes = size as u64;
    let result = match freeing(free_type) {
        // With no size, all of the part that starts there.
        Some(Freeing::Decommit) if size == 0 => memory()
            .part(at)
            .ok_or(riptide_mem::MemError::InvalidAddress)
            .and_then(|bytes| memory().unmap(at, bytes)),
        Some(Freeing::Decommit) => memory().unmap(at, bytes),
        Some(Freeing::Release) if size == 0 => memory().release(at),
        Some(Freeing::Cut) => memory().cut(at, bytes),
        Some(Freeing::Join) => memory().uncut(at, bytes),
        Some(Freeing::Release) | None => Err(riptide_mem::MemError::InvalidParameter),
    };
    if free_type != MEM_DECOMMIT || result.is_err() {
        trace!(
            "VirtualFree({address:p}, size {size:#x}, type {free_type:#x}) on title memory{}",
            match result {
                Ok(()) => String::new(),
                Err(error) => format!(": {error}"),
            }
        );
    }
    if result.is_err() {
        // SAFETY: the call has no preconditions.
        unsafe { sys::SetLastError(ERROR_INVALID_ADDRESS) };
    }
    i32::from(result.is_ok())
}

type AllocRoutine = unsafe extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = unsafe extern "system" fn(*mut c_void, u64);

/// The title's own allocation and free routines, which it exports as
/// `__XMemAllocationHooks__` if it has them. `XMemAlloc` and `XMemFree` go to them, for the
/// title's calls and its middleware's alike.
pub(crate) fn hooks() -> Option<(AllocRoutine, FreeRoutine)> {
    let at = crate::process::title_export("__XMemAllocationHooks__")?;
    // SAFETY: the export is two routine addresses, in the title's mapped image.
    let [alloc, free] = unsafe { (at as *const [usize; 2]).read_unaligned() };
    if alloc == 0 || free == 0 {
        return None;
    }
    // SAFETY: the title declares routines of these types there.
    Some(unsafe {
        (
            std::mem::transmute::<usize, AllocRoutine>(alloc),
            std::mem::transmute::<usize, FreeRoutine>(free),
        )
    })
}

/// `xmem.dll!XMemAlloc`: the title's general allocator: its own hook if it has one, else
/// [`alloc_default`].
///
/// # Safety
/// The arguments are the title's own.
pub unsafe extern "system" fn alloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks() {
        // SAFETY: the title's own routine, called as the console calls it.
        Some((alloc, _)) => unsafe { alloc(size, attributes) },
        // SAFETY: as received.
        None => unsafe { alloc_default(size, attributes) },
    }
}

/// `xmem.dll!XMemFree`.
///
/// # Safety
/// `address` came from [`alloc`] with the same attributes.
pub unsafe extern "system" fn free(address: *mut c_void, attributes: u64) {
    match hooks() {
        // SAFETY: the title's own routine, called as the console calls it.
        Some((_, free)) => unsafe { free(address, attributes) },
        // SAFETY: as received.
        None => unsafe { free_default(address, attributes) },
    }
}

/// The alignment `attributes` ask for: at least 16 bytes.
fn alignment(attributes: u64) -> usize {
    1usize << ((attributes >> ATTRIBUTE_ALIGNMENT_SHIFT) & 0x1F).max(4)
}

/// The kind of memory `attributes` ask for, from bit 29: zero is the ordinary heap, and
/// every other kind is whole pages for the GPU or of physical memory.
fn memory_type(attributes: u64) -> u64 {
    (attributes >> ATTRIBUTE_MEMORY_TYPE_SHIFT) & 0xF
}

/// `xmem.dll!XMemAllocDefault`: the system's allocator behind `XMemAlloc`. Memory that must
/// lie below 4 GiB, and memory of a kind the GPU uses, is title memory of its own; anything
/// else comes from the C runtime's heap, which is not memory the GPU uses in place.
///
/// # Safety
/// The arguments are the title's own.
pub unsafe extern "system" fn alloc_default(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ATTRIBUTE_32BIT == 0 && memory_type(attributes) != 0 {
        // Graphics memory: pages of its own, below 2^37, where the GPU's page table ends.
        let alignment = alignment(attributes) as u64;
        let limits = Limits {
            lowest: 4 * GIB,
            highest: (1 << 37) - 1,
            alignment: if alignment > FRAME { alignment } else { 0 },
        };
        let result = memory().commit(None, size.max(1) as u64, limits, Access::ReadWrite);
        let shown = GRAPHICS_SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if shown < 8 || result.is_err() {
            trace!(
                "XMemAllocDefault({size:#x} bytes, attributes {attributes:#x}): graphics \
                 memory {}",
                match result {
                    Ok(address) => format!("at {address:#x}"),
                    Err(error) => format!("refused: {error}"),
                }
            );
        }
        return result.unwrap_or(0) as *mut c_void;
    }
    if attributes & ATTRIBUTE_32BIT != 0 {
        // SAFETY: a new allocation of our choosing.
        return unsafe {
            virtual_alloc(
                std::ptr::null_mut(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                FLAG_32BIT,
                PAGE_READWRITE,
            )
        };
    }
    // SAFETY: the C runtime's aligned allocator takes any size and a power-of-two alignment.
    unsafe { sys::_aligned_malloc(size.max(1), alignment(attributes)) }
}

/// `xmem.dll!XMemFreeDefault`.
///
/// # Safety
/// `address` is null or came from [`alloc_default`] with the same attributes.
pub unsafe extern "system" fn free_default(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    if attributes & ATTRIBUTE_32BIT != 0 || memory_type(attributes) != 0 {
        if let Err(error) = memory().release(address as u64) {
            trace!("XMemFreeDefault({address:p}): {error}");
        }
    } else {
        // SAFETY: the block came from `_aligned_malloc`.
        unsafe { sys::_aligned_free(address) };
    }
}

/// `xmem.dll!XMemIsToolsMemAvailable`: a retail console has no tools memory.
pub extern "system" fn is_tools_mem_available() -> i32 {
    0
}

/// The frames behind console page numbers, or `None` if one is not a frame's number.
fn frames_of(pages: &[usize]) -> Option<Vec<u32>> {
    pages
        .iter()
        .map(|&page| {
            (page != 0 && page.is_multiple_of(PAGES_PER_FRAME))
                .then(|| u32::try_from(page / PAGES_PER_FRAME).ok())
                .flatten()
        })
        .collect()
}

/// `xmem.dll!XMemAllocatePhysicalPages`: `count` 64 KiB pages, by number. `count` goes in
/// as the pages wanted and comes out as the pages given: all or none.
///
/// # Safety
/// `count` is null or the title's count; `pages` is null or room for that many numbers.
pub unsafe extern "system" fn allocate_physical_pages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> i32 {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null; the title's own count.
    let wanted = unsafe { count.read() };
    let frames = u32::try_from(wanted)
        .ok()
        .and_then(|wanted| memory().allocate_frames(wanted).ok());
    trace!(
        "XMemAllocatePhysicalPages(flags {flags:#x}, {wanted} pages) {}",
        if frames.is_some() { "given" } else { "refused" }
    );
    let Some(frames) = frames else {
        // SAFETY: checked non-null.
        unsafe {
            count.write(0);
            sys::SetLastError(ERROR_NOT_ENOUGH_MEMORY);
        }
        return 0;
    };
    // SAFETY: the title passes room for `wanted` numbers.
    let out = unsafe { std::slice::from_raw_parts_mut(pages, wanted) };
    for (slot, frame) in out.iter_mut().zip(frames) {
        *slot = frame as usize * PAGES_PER_FRAME;
    }
    1
}

/// `xmem.dll!XMemFreePhysicalPages`. A page that is still mapped stays readable there until
/// it is unmapped.
///
/// # Safety
/// `pages` is null or `count` page numbers.
pub unsafe extern "system" fn free_physical_pages(count: usize, pages: *const usize) -> i32 {
    if pages.is_null() {
        return 0;
    }
    // SAFETY: the title passes `count` numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, count) };
    let freed = frames_of(pages).is_some_and(|frames| memory().free_frames(&frames).is_ok());
    if !freed {
        trace!("XMemFreePhysicalPages({count} pages) refused: not pages the title holds");
    }
    i32::from(freed)
}

/// `xmem.dll!XMemMapPhysicalPages`: maps `count` pages at `address`, one after another, in
/// place of what is mapped there. Null `pages` unmaps. Returns `address`, or null.
///
/// # Safety
/// `address` lies in a reservation the title made; `pages` is null or `count` numbers.
pub unsafe extern "system" fn map_physical_pages(
    address: *mut c_void,
    count: usize,
    pages: *const usize,
) -> *mut c_void {
    let at = address as u64;
    let result = if pages.is_null() {
        memory().unmap(at, count as u64 * FRAME)
    } else {
        // SAFETY: the title passes `count` numbers.
        let pages = unsafe { std::slice::from_raw_parts(pages, count) };
        match frames_of(pages) {
            Some(frames) => memory().map_frames(at, &frames, Access::ReadWrite),
            None => Err(riptide_mem::MemError::InvalidParameter),
        }
    };
    match result {
        Ok(()) => address,
        Err(error) => {
            trace!("XMemMapPhysicalPages({address:p}, {count} pages): {error}");
            std::ptr::null_mut()
        }
    }
}

/// `xmem.dll!XMemMapPhysicalPagesScatter`: one page at each address. Null `pages`, or a
/// page number of zero, unmaps that address.
///
/// # Safety
/// `addresses` is null or `count` addresses in reservations the title made; `pages` is null
/// or `count` numbers.
pub unsafe extern "system" fn map_physical_pages_scatter(
    addresses: *const *mut c_void,
    count: usize,
    pages: *const usize,
) -> i32 {
    if addresses.is_null() {
        return 0;
    }
    // SAFETY: the title passes `count` addresses, and `count` numbers or null.
    let (addresses, pages) = unsafe {
        (
            std::slice::from_raw_parts(addresses, count),
            (!pages.is_null()).then(|| std::slice::from_raw_parts(pages, count)),
        )
    };
    let mut all = true;
    for (index, &address) in addresses.iter().enumerate() {
        let page = pages.map_or(0, |pages| pages[index]);
        let at = address as u64;
        let done = if page == 0 {
            memory().unmap(at, FRAME)
        } else {
            match frames_of(&[page]) {
                Some(frame) => memory().map_frames(at, &frame, Access::ReadWrite),
                None => Err(riptide_mem::MemError::InvalidParameter),
            }
        };
        if let Err(error) = done {
            trace!("XMemMapPhysicalPagesScatter({address:p}, page {page:#x}): {error}");
            all = false;
        }
    }
    i32::from(all)
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, &str, usize); 12] = [
        (
            "kernel32.dll",
            "VirtualFree",
            virtual_free as *const () as usize,
        ),
        (
            "d3d12_xs.dll",
            "D3D12ConfigureVirtualMemoryX",
            configure_virtual_memory as *const () as usize,
        ),
        (
            "xmem.dll",
            "XMemVirtualAlloc",
            virtual_alloc as *const () as usize,
        ),
        ("xmem.dll", "XMemAlloc", alloc as *const () as usize),
        ("xmem.dll", "XMemFree", free as *const () as usize),
        (
            "xmem.dll",
            "XMemAllocDefault",
            alloc_default as *const () as usize,
        ),
        (
            "xmem.dll",
            "XMemFreeDefault",
            free_default as *const () as usize,
        ),
        (
            "xmem.dll",
            "XMemIsToolsMemAvailable",
            is_tools_mem_available as *const () as usize,
        ),
        (
            "xmem.dll",
            "XMemAllocatePhysicalPages",
            allocate_physical_pages as *const () as usize,
        ),
        (
            "xmem.dll",
            "XMemFreePhysicalPages",
            free_physical_pages as *const () as usize,
        ),
        (
            "xmem.dll",
            "XMemMapPhysicalPages",
            map_physical_pages as *const () as usize,
        ),
        (
            "xmem.dll",
            "XMemMapPhysicalPagesScatter",
            map_physical_pages_scatter as *const () as usize,
        ),
    ];
    for (module, name, address) in functions {
        // SAFETY: each function above has the signature of the console function it is
        // registered as.
        unsafe { imports.implement(module, Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_set_the_address_limits() {
        let low = limits(FLAG_32BIT, 0);
        assert_eq!((low.lowest, low.highest), (0x1_0000, 0xFFFF_FFFF));
        // The flags allow up to 2^40 or more, and all of that is given below 2^37, where
        // the GPU's page table ends.
        assert_eq!(limits(0, 0).highest, (1 << 37) - 1);
        assert_eq!(limits(3, 0).lowest, 4 * GIB);
        assert_eq!(limits(1 << 12, 0).highest, (1 << 37) - 1);
        assert_eq!(limits(7 << 12, 0).highest, (1 << 37) - 1);
        assert_eq!(limits(0, 2 * MIB).alignment, 2 * MIB);
    }

    #[test]
    fn protection_values_become_how_the_cpu_reaches_memory() {
        assert_eq!(access(0x04), Access::ReadWrite);
        assert_eq!(access(0x22004), Access::ReadWrite);
        assert_eq!(access(0x801), Access::None);
        assert_eq!(access(0x02), Access::ReadOnly);
        assert_eq!(access(0), Access::None);
    }

    #[test]
    fn committed_memory_is_title_memory_inside_its_limits() {
        for (flags, kind) in [
            (0x183u64, MEM_RESERVE | MEM_COMMIT | PAGES_2M),
            (FLAG_32BIT, MEM_RESERVE | MEM_COMMIT),
        ] {
            let size = 4 * MIB as usize;
            // SAFETY: a new allocation, written inside its size.
            unsafe {
                let at = virtual_alloc(std::ptr::null_mut(), size, kind, flags, 0x22004);
                assert!(!at.is_null(), "flags {flags:#x} kind {kind:#x}");
                let within = limits(flags, 0);
                assert!(
                    at as u64 >= within.lowest && at as u64 + size as u64 - 1 <= within.highest
                );
                if kind & PAGES_2M != 0 {
                    assert_eq!(at as u64 % (2 * MIB), 0);
                }
                at.cast::<u8>().add(size - 1).write(7);
                // It lies in a bank: the GPU can use it where it is.
                let parts = memory().locate(at as u64, size as u64).unwrap();
                assert_eq!(
                    parts.iter().map(|part| part.bytes).sum::<u64>(),
                    size as u64
                );
                memory().release(at as u64).unwrap();
            }
        }
    }

    #[test]
    fn physical_pages_are_numbered_mapped_unmapped_and_freed() {
        // SAFETY: valid in- and out-pointers, and addresses inside the reservation made.
        unsafe {
            let range = virtual_alloc(
                std::ptr::null_mut(),
                64 * FRAME as usize,
                MEM_RESERVE | PAGES_2M | PAGES_64K_BIT,
                0x3,
                0x801,
            );
            assert!(!range.is_null());
            // The title cuts a part off with `VirtualFree` and then takes it. A part it has
            // not cut is not there to take.
            let take = |size: u64| {
                virtual_alloc(
                    range,
                    size as usize,
                    MEM_RESERVE | MEM_REPLACE_PLACEHOLDER | PAGES_2M | PAGES_64K_BIT,
                    0x3,
                    0x801,
                )
            };
            assert!(take(FRAME).is_null());
            let cut = MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER;
            assert_eq!(virtual_free(range, FRAME as usize, cut), 1);
            assert_eq!(take(FRAME), range);
            assert!(take(2 * FRAME).is_null());

            let mut count = 3usize;
            let mut pages = [0usize; 3];
            assert_eq!(
                allocate_physical_pages(0, &mut count, pages.as_mut_ptr()),
                1
            );
            assert_eq!(count, 3);
            assert!(
                pages
                    .iter()
                    .all(|&page| page != 0 && page % PAGES_PER_FRAME == 0)
            );

            let at = range.cast::<u8>().add(4 * FRAME as usize);
            assert_eq!(map_physical_pages(at.cast(), 3, pages.as_ptr()), at.cast());
            at.add(2 * FRAME as usize + 9).write(0x42);

            // One of them again at another address: the same memory.
            let alias = range.cast::<u8>().add(32 * FRAME as usize);
            let addresses = [alias.cast::<c_void>()];
            assert_eq!(
                map_physical_pages_scatter(addresses.as_ptr(), 1, &pages[2]),
                1
            );
            assert_eq!(alias.add(9).read(), 0x42);

            // A zero page number unmaps; a number that is no frame's is refused.
            assert_eq!(map_physical_pages_scatter(addresses.as_ptr(), 1, &0), 1);
            assert!(memory().locate(alias as u64, 1).is_none());
            assert_eq!(map_physical_pages_scatter(addresses.as_ptr(), 1, &7), 0);

            assert_eq!(
                map_physical_pages(at.cast(), 3, std::ptr::null()),
                at.cast()
            );
            assert_eq!(free_physical_pages(3, pages.as_ptr()), 1);
            assert_eq!(free_physical_pages(3, pages.as_ptr()), 0);

            // Each part is given back by itself, and then is title memory no more.
            let rest = range.cast::<u8>().add(FRAME as usize).cast::<c_void>();
            assert_eq!(virtual_free(range, 0, MEM_RELEASE), 1);
            assert!(!memory().holds(range as u64) && memory().holds(rest as u64));
            assert_eq!(virtual_free(rest, FRAME as usize, MEM_RELEASE), 0);
            assert_eq!(virtual_free(rest, 0, MEM_RELEASE), 1);
            assert!(!memory().holds(rest as u64));
        }
    }

    #[test]
    fn a_free_type_says_what_a_free_of_title_memory_does() {
        assert_eq!(freeing(0x4000), Some(Freeing::Decommit));
        assert_eq!(freeing(0x8000), Some(Freeing::Release));
        assert_eq!(freeing(0x8002), Some(Freeing::Cut));
        assert_eq!(freeing(0x8001), Some(Freeing::Join));
        assert_eq!(freeing(0xC000), None);
        assert_eq!(freeing(0), None);
    }

    #[test]
    fn a_free_of_memory_that_is_not_the_titles_goes_to_windows() {
        // SAFETY: a block from Windows, freed once, through the function under test.
        unsafe {
            let block = sys::VirtualAlloc(std::ptr::null_mut(), 0x1_0000, 0x3000, PAGE_READWRITE);
            assert!(!block.is_null());
            assert!(!memory().holds(block as u64));
            assert_eq!(virtual_free(block.cast(), 0, MEM_RELEASE), 1);
        }
    }

    #[test]
    fn the_default_allocator_aligns_as_asked() {
        // SAFETY: blocks are freed with the attributes they were made with.
        unsafe {
            for log2 in [0u64, 4, 6, 12, 16] {
                let attributes = log2 << ATTRIBUTE_ALIGNMENT_SHIFT;
                let block = alloc_default(100, attributes);
                assert!(!block.is_null());
                assert_eq!(block as usize % (1usize << log2.max(4)), 0);
                block.cast::<u8>().add(99).write(1);
                free_default(block, attributes);
            }
            // Graphics memory is title memory, below 2^37, aligned as asked.
            let attributes = (1 << ATTRIBUTE_MEMORY_TYPE_SHIFT) | (21 << ATTRIBUTE_ALIGNMENT_SHIFT);
            let graphics = alloc_default(0x3_0000, attributes);
            assert!(memory().holds(graphics as u64));
            assert!((4 * GIB..1 << 37).contains(&(graphics as u64)));
            assert_eq!(graphics as u64 % (2 * MIB), 0);
            assert!(memory().locate(graphics as u64, 0x3_0000).is_some());
            free_default(graphics, attributes);
            assert!(!memory().holds(graphics as u64));

            let low = alloc_default(0x1_0000, ATTRIBUTE_32BIT);
            assert!(!low.is_null() && (low as u64) < 4 * GIB);
            free_default(low, ATTRIBUTE_32BIT);
        }
    }
}
