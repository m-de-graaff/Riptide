//! The Windows calls title memory is made of: sections, placeholders and views.
//!
//! A reservation is a placeholder: address space that holds nothing and can be split into
//! pieces, each of which a view of a section can take the place of. Unmapping the view
//! leaves the placeholder behind.

use std::ffi::c_void;
use std::io;
use std::ops::Range;

pub(crate) const MEM_RESERVE: u32 = 0x2000;
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
const MEM_RESERVE_PLACEHOLDER: u32 = 0x4_0000;
const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
const MEM_PRESERVE_PLACEHOLDER: u32 = 0x2;

pub(crate) const PAGE_NOACCESS: u32 = 0x01;
pub(crate) const PAGE_READONLY: u32 = 0x02;
pub(crate) const PAGE_READWRITE: u32 = 0x04;
const SEC_COMMIT: u32 = 0x800_0000;
const FILE_MAP_ALL_ACCESS: u32 = 0xF001F;

/// `MEM_ADDRESS_REQUIREMENTS`: where `VirtualAlloc2` may place memory.
#[repr(C)]
struct AddressRequirements {
    lowest: *mut c_void,
    highest: *mut c_void,
    alignment: usize,
}

/// `MEM_EXTENDED_PARAMETER` holding a pointer; its type is the low bits of the first word.
#[repr(C)]
struct ExtendedParameter {
    kind: u64,
    pointer: *mut c_void,
}

const PARAMETER_ADDRESS_REQUIREMENTS: u64 = 1;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileMappingW(
        file: *mut c_void,
        attributes: *const c_void,
        protection: u32,
        size_high: u32,
        size_low: u32,
        name: *const u16,
    ) -> *mut c_void;
    fn MapViewOfFile(
        mapping: *mut c_void,
        access: u32,
        offset_high: u32,
        offset_low: u32,
        bytes: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(address: *const c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    fn VirtualProtect(address: *mut c_void, size: usize, protection: u32, old: *mut u32) -> i32;
    /// Throws away the contents of committed pages and frees the RAM behind them at once.
    /// Returns zero on success.
    fn DiscardVirtualMemory(address: *mut c_void, size: usize) -> u32;
}

// These have no entry in kernel32's import library; they live in kernelbase.
#[link(name = "kernelbase", kind = "raw-dylib")]
unsafe extern "system" {
    fn VirtualAlloc2(
        process: *mut c_void,
        base: *mut c_void,
        size: usize,
        allocation_type: u32,
        protection: u32,
        parameters: *mut ExtendedParameter,
        parameter_count: u32,
    ) -> *mut c_void;
    fn MapViewOfFile3(
        mapping: *mut c_void,
        process: *mut c_void,
        base: *mut c_void,
        offset: u64,
        size: usize,
        allocation_type: u32,
        protection: u32,
        parameters: *mut ExtendedParameter,
        parameter_count: u32,
    ) -> *mut c_void;
    fn UnmapViewOfFile2(process: *mut c_void, base: *mut c_void, flags: u32) -> i32;
}

/// Where a reservation may lie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The lowest address it may start at.
    pub lowest: u64,
    /// The highest address its last byte may have.
    pub highest: u64,
    /// What its start must be a multiple of. Zero: Windows' own 64 KiB.
    pub alignment: u64,
}

/// Reserves `bytes` of address space as one placeholder inside `limits`.
pub(crate) fn reserve(bytes: u64, limits: Limits) -> io::Result<u64> {
    let mut requirements = AddressRequirements {
        lowest: limits.lowest as *mut c_void,
        highest: limits.highest as *mut c_void,
        alignment: limits.alignment as usize,
    };
    let mut parameter = ExtendedParameter {
        kind: PARAMETER_ADDRESS_REQUIREMENTS,
        pointer: (&raw mut requirements).cast(),
    };
    // SAFETY: a new reservation; the parameter points at `requirements`, which outlives the
    // call.
    let address = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null_mut(),
            bytes as usize,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            &raw mut parameter,
            1,
        )
    };
    if address.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(address as u64)
}

/// Cuts `piece` out of the placeholder it lies in, so that it is a placeholder of its own.
///
/// # Safety
/// `piece` lies inside one placeholder this process reserved, and is smaller than it.
pub(crate) unsafe fn split(piece: Range<u64>) -> io::Result<()> {
    // SAFETY: as this function's contract says.
    let done = unsafe {
        VirtualFree(
            piece.start as *mut c_void,
            (piece.end - piece.start) as usize,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Joins the placeholders that together make up exactly `whole` into one.
///
/// # Safety
/// `whole` is covered exactly by two or more neighbouring placeholders of this process.
pub(crate) unsafe fn join(whole: Range<u64>) -> io::Result<()> {
    // SAFETY: as this function's contract says.
    let done = unsafe {
        VirtualFree(
            whole.start as *mut c_void,
            (whole.end - whole.start) as usize,
            MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS,
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Gives a whole placeholder's addresses back.
///
/// # Safety
/// `address` starts a placeholder of this process that nothing uses.
pub(crate) unsafe fn release(address: u64) -> io::Result<()> {
    // SAFETY: as this function's contract says.
    if unsafe { VirtualFree(address as *mut c_void, 0, MEM_RELEASE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Takes a mapped view away and leaves a placeholder of its size in its place.
///
/// # Safety
/// `address` starts a view mapped by [`Section::map`], which nothing uses any more.
pub(crate) unsafe fn unmap(address: u64) -> io::Result<()> {
    // SAFETY: as this function's contract says.
    let done = unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            address as *mut c_void,
            MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets the protection of whole pages.
///
/// # Safety
/// `pages` is committed memory of this process, and no code relies on its old protection.
pub(crate) unsafe fn protect(pages: Range<u64>, protection: u32) -> io::Result<()> {
    let mut old = 0u32;
    // SAFETY: as this function's contract says.
    let done = unsafe {
        VirtualProtect(
            pages.start as *mut c_void,
            (pages.end - pages.start) as usize,
            protection,
            &mut old,
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A committed, page-file-backed section and the one view of all of it that stays mapped.
pub(crate) struct Section {
    handle: *mut c_void,
    anchor: *mut u8,
    bytes: u64,
}

// SAFETY: the handle and the view belong to the process, not to a thread. `Section` hands
// out no references into the view.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Section {
    /// Creates a section of `bytes` and maps its anchor. All of it is charged against the
    /// page file at once; only pages that are touched use RAM.
    pub(crate) fn create(bytes: u64) -> io::Result<Section> {
        // SAFETY: plain calls with valid arguments. The handle and the view pass to
        // `Section` at once, so its `Drop` releases them on every path.
        unsafe {
            let handle = CreateFileMappingW(
                usize::MAX as *mut c_void,
                std::ptr::null(),
                PAGE_READWRITE | SEC_COMMIT,
                (bytes >> 32) as u32,
                bytes as u32,
                std::ptr::null(),
            );
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut section = Section {
                handle,
                anchor: std::ptr::null_mut(),
                bytes,
            };
            section.anchor =
                MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize).cast();
            if section.anchor.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(section)
        }
    }

    /// Where the anchor view starts.
    pub(crate) fn anchor(&self) -> *mut u8 {
        self.anchor
    }

    /// Maps `bytes` of the section from `offset` in the place of the placeholder that is
    /// exactly `at..at + bytes`.
    ///
    /// # Safety
    /// That range is one whole placeholder of this process.
    pub(crate) unsafe fn map(
        &self,
        at: u64,
        offset: u64,
        bytes: u64,
        protection: u32,
    ) -> io::Result<()> {
        assert!(offset + bytes <= self.bytes);
        // SAFETY: as this function's contract says.
        let view = unsafe {
            MapViewOfFile3(
                self.handle,
                GetCurrentProcess(),
                at as *mut c_void,
                offset,
                bytes as usize,
                MEM_REPLACE_PLACEHOLDER,
                protection,
                std::ptr::null_mut(),
                0,
            )
        };
        if view.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Gives the RAM behind `bytes` from `offset` back to Windows. What they hold afterwards
    /// is not defined: use [`Section::zero`] before handing them out again. Returns false if
    /// Windows refused, in which case their RAM stays in use.
    ///
    /// Windows discards a shared page only through a view that has touched it, so each page
    /// is read through the anchor first.
    ///
    /// # Safety
    /// Nothing uses those bytes: no view of them is mapped but the anchor.
    pub(crate) unsafe fn discard(&self, offset: u64, bytes: u64) -> bool {
        assert!(offset + bytes <= self.bytes);
        // SAFETY: the range lies inside the anchor view, which is readable and writable.
        unsafe {
            let start = self.anchor.add(offset as usize);
            let mut page = 0;
            while page < bytes as usize {
                start.add(page).read_volatile();
                page += 4096;
            }
            DiscardVirtualMemory(start.cast(), bytes as usize) == 0
        }
    }

    /// Fills `bytes` from `offset` with zeros.
    ///
    /// # Safety
    /// Nothing uses those bytes: no view of them is mapped but the anchor.
    pub(crate) unsafe fn zero(&self, offset: u64, bytes: u64) {
        assert!(offset + bytes <= self.bytes);
        // SAFETY: the range lies inside the anchor view, which is readable and writable.
        unsafe { std::ptr::write_bytes(self.anchor.add(offset as usize), 0, bytes as usize) };
    }
}
impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view and the handle are released exactly once, here. A D3D12 heap
        // opened on the anchor has to be gone by now; `TitleMemory` documents that.
        unsafe {
            if !self.anchor.is_null() {
                UnmapViewOfFile(self.anchor.cast());
            }
            CloseHandle(self.handle);
        }
    }
}
