//! The console's memory functions: `xmem.dll`, and the one graphics call that sets up
//! addresses before any of them, `D3D12ConfigureVirtualMemoryX`.
//!
//! On the console the CPU and the GPU share one memory and one set of addresses, so a
//! graphics address is the title's own pointer. The console's flags say more than Windows'
//! do: a page size (4 KiB, 64 KiB or 2 MiB), how many address bits the memory may use, and
//! which processor reaches it how. Here the address limits and alignments are kept, because
//! the title computes with them; the rest is noted and dropped.
//!
//! Not built yet: this memory is ordinary host memory, not yet part of the block the GPU
//! uses in place (`riptide-mem`).

use std::ffi::c_void;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::sys;
use crate::trace::trace;

const MIB: usize = 1 << 20;
const GIB: usize = 1 << 30;

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
const MEM_RESERVE_PLACEHOLDER: u32 = 0x4_0000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READWRITE: u32 = 0x04;

/// Allocation-type bit: 2 MiB pages. Windows reads this bit as large pages.
const PAGES_2M: u32 = 0x2000_0000;
/// Allocation-type bit that, with [`PAGES_2M`], means 64 KiB pages.
const PAGES_64K_BIT: u32 = 0x40_0000;
/// Windows' protection values. The console's bits above them say how the GPU reaches the
/// memory.
const HOST_PROTECTION: u32 = 0x1FF;

/// `XMemVirtualAlloc` flag: the memory must lie below 4 GiB.
const FLAG_32BIT: u64 = 1 << 2;
/// `XMemAlloc` attributes: the alignment as a power of two, in five bits from bit 24.
const ATTRIBUTE_ALIGNMENT_SHIFT: u64 = 24;
/// `XMemAlloc` attribute: the memory must lie below 4 GiB.
const ATTRIBUTE_32BIT: u64 = 1 << 33;

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

/// The lowest and highest address `flags` allow. Memory marked 32-bit lies below 4 GiB.
/// Other memory lies above it, so the low 4 GiB stay free for the 32-bit requests, and below
/// a limit the three bits from bit 12 give: 46 bits for 1 down to 40 for 7, and 40 with no
/// field. The title computes with these limits (its ray-tracing structures must lie below
/// 2^40).
fn address_range(flags: u64) -> (usize, usize) {
    if flags & FLAG_32BIT != 0 {
        return (0x1_0000, u32::MAX as usize);
    }
    let bits = match (flags >> 12) & 7 {
        0 => 40,
        field => 47 - field,
    };
    (4 * GIB, (1usize << bits) - 1)
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
    let pages_64k = allocation_type & (PAGES_2M | PAGES_64K_BIT) == (PAGES_2M | PAGES_64K_BIT);
    let pages_2m = allocation_type & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M;
    // A reservation of 64 KiB pages is filled with physical pages later
    // (`XMemMapPhysicalPages`). On Windows only a placeholder can take a mapping.
    let placeholder = pages_64k && allocation_type & MEM_COMMIT == 0;
    if placeholder && allocation_type & MEM_REPLACE_PLACEHOLDER != 0 {
        // The title is carving up a reservation it already has; it stays a placeholder.
        return base;
    }

    let mut kind = allocation_type & !(PAGES_2M | PAGES_64K_BIT);
    let mut host_protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        protection => protection,
    };
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
        host_protect = PAGE_NOACCESS;
    }

    let (lowest, highest) = address_range(flags);
    let mut requirements = sys::MEM_ADDRESS_REQUIREMENTS {
        lowest_starting_address: lowest as *mut c_void,
        highest_ending_address: highest as *mut c_void,
        alignment: if pages_2m { 2 * MIB } else { 0 },
    };
    let mut parameter = sys::MEM_EXTENDED_PARAMETER {
        kind: sys::MEM_EXTENDED_PARAMETER_ADDRESS_REQUIREMENTS,
        pointer: (&raw mut requirements).cast(),
    };
    // The limits apply where the address is ours to choose.
    let ours_to_place = base.is_null();
    if ours_to_place {
        kind |= MEM_RESERVE;
    }
    // SAFETY: the title's arguments with the console's bits taken out; the parameter points
    // at `requirements`, which outlives the call.
    let address = unsafe {
        sys::VirtualAlloc2(
            sys::GetCurrentProcess(),
            base,
            size,
            kind,
            host_protect,
            if ours_to_place {
                &raw mut parameter
            } else {
                std::ptr::null_mut()
            },
            u32::from(ours_to_place),
        )
    };
    trace!(
        "XMemVirtualAlloc(base {base:p}, size {size:#x}, type {allocation_type:#x}, flags \
         {flags:#x}, protect {protect:#x}) = {address:p}{}",
        if address.is_null() {
            format!(": {}", std::io::Error::last_os_error())
        } else {
            String::new()
        }
    );
    address
}

type AllocRoutine = unsafe extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = unsafe extern "system" fn(*mut c_void, u64);

/// The title's own allocation and free routines, which it exports as
/// `__XMemAllocationHooks__` if it has them. `XMemAlloc` and `XMemFree` go to them, for the
/// title's calls and its middleware's alike.
fn hooks() -> Option<(AllocRoutine, FreeRoutine)> {
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

/// `xmem.dll!XMemAllocDefault`: the system's allocator behind `XMemAlloc`.
///
/// # Safety
/// The arguments are the title's own.
pub unsafe extern "system" fn alloc_default(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ATTRIBUTE_32BIT != 0 {
        // SAFETY: a new reservation of our choosing.
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
    if attributes & ATTRIBUTE_32BIT != 0 {
        // SAFETY: a whole reservation `alloc_default` made.
        unsafe { sys::VirtualFree(address, 0, MEM_RELEASE) };
    } else {
        // SAFETY: the block came from `_aligned_malloc`.
        unsafe { sys::_aligned_free(address) };
    }
}

/// `xmem.dll!XMemIsToolsMemAvailable`: a retail console has no tools memory.
pub extern "system" fn is_tools_mem_available() -> i32 {
    0
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, &str, usize); 7] = [
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
        assert_eq!(address_range(FLAG_32BIT), (0x1_0000, 0xFFFF_FFFF));
        assert_eq!(address_range(0), (4 * GIB, (1 << 40) - 1));
        assert_eq!(address_range(3), (4 * GIB, (1 << 40) - 1));
        assert_eq!(address_range(1 << 12), (4 * GIB, (1 << 46) - 1));
        assert_eq!(address_range(7 << 12), (4 * GIB, (1 << 40) - 1));
    }

    #[test]
    fn memory_is_placed_inside_its_limits_and_can_be_used() {
        for (flags, kind) in [
            (0u64, MEM_RESERVE | MEM_COMMIT),
            (FLAG_32BIT, MEM_RESERVE | MEM_COMMIT),
            (3, MEM_RESERVE | MEM_COMMIT | PAGES_2M),
        ] {
            let size = 4 * MIB;
            // SAFETY: a new allocation, written inside its size and released whole.
            unsafe {
                let at = virtual_alloc(
                    std::ptr::null_mut(),
                    size,
                    kind,
                    flags,
                    PAGE_READWRITE | 0x800,
                );
                assert!(!at.is_null(), "flags {flags:#x} kind {kind:#x}");
                let (lowest, highest) = address_range(flags);
                assert!(at as usize >= lowest && at as usize + size - 1 <= highest);
                if kind & PAGES_2M != 0 {
                    assert_eq!(at as usize % (2 * MIB), 0);
                }
                at.cast::<u8>().add(size - 1).write(7);
                assert_ne!(sys::VirtualFree(at, 0, MEM_RELEASE), 0);
            }
        }
    }

    #[test]
    fn a_reservation_of_64_kib_pages_is_left_for_physical_pages() {
        // SAFETY: a reservation only; nothing is read or written.
        unsafe {
            let at = virtual_alloc(
                std::ptr::null_mut(),
                4 * MIB,
                MEM_RESERVE | PAGES_2M | PAGES_64K_BIT,
                3,
                PAGE_READWRITE,
            );
            assert!(!at.is_null());
            // Carving it up is answered without asking Windows.
            let part = virtual_alloc(
                at,
                MIB,
                MEM_RESERVE | MEM_REPLACE_PLACEHOLDER | PAGES_2M | PAGES_64K_BIT,
                3,
                PAGE_READWRITE,
            );
            assert_eq!(part, at);
            assert_ne!(sys::VirtualFree(at, 0, MEM_RELEASE), 0);
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
            let low = alloc_default(0x1_0000, ATTRIBUTE_32BIT);
            assert!(!low.is_null() && (low as usize) < 4 * GIB);
            free_default(low, ATTRIBUTE_32BIT);
        }
    }
}
