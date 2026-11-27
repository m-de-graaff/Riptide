//! The block itself: one committed section of RAM and the process's one view of it.

use crate::pages::MemError;
use std::ops::Range;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_NOACCESS, PAGE_PROTECTION_FLAGS, PAGE_READWRITE, SEC_COMMIT, UnmapViewOfFile,
    VirtualProtect,
};
use windows::core::PCWSTR;

#[link(name = "kernel32")]
unsafe extern "system" {
    /// Throws away the contents of committed pages and frees the RAM behind them at once.
    /// The pages stay committed and read as zero afterwards. Returns zero on success.
    fn DiscardVirtualMemory(address: *mut core::ffi::c_void, size: usize) -> u32;
}

fn os(e: windows::core::Error) -> MemError {
    MemError::Os(e.code().0 as u32)
}

pub(crate) struct Block {
    handle: HANDLE,
    view: *mut u8,
    bytes: u64,
}

// SAFETY: the handle and the view belong to the process, not to a thread. `Block` hands out
// no references into the view; callers change pages only while holding the allocator's lock.
unsafe impl Send for Block {}
// SAFETY: as above.
unsafe impl Sync for Block {}

impl Block {
    /// Creates the section and maps it, with every page inaccessible until it is committed.
    ///
    /// The section is committed, so all of `bytes` is charged against the page file at once,
    /// though only pages that are touched use RAM.
    pub fn create(bytes: u64) -> Result<Block, MemError> {
        // SAFETY: plain Win32 calls with valid arguments. The handle and the view pass to
        // `Block` at once, so its `Drop` releases them on every path.
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE | SEC_COMMIT,
                (bytes >> 32) as u32,
                bytes as u32,
                PCWSTR::null(),
            )
            .map_err(os)?;
            let mut block = Block {
                handle,
                view: std::ptr::null_mut(),
                bytes,
            };
            block.view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize)
                .Value
                .cast();
            if block.view.is_null() {
                return Err(os(windows::core::Error::from_thread()));
            }
            block.protect(0..bytes, PAGE_NOACCESS)?;
            Ok(block)
        }
    }

    pub fn base(&self) -> u64 {
        self.view as u64
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    fn protect(
        &self,
        range: Range<u64>,
        protection: PAGE_PROTECTION_FLAGS,
    ) -> Result<(), MemError> {
        assert!(range.start <= range.end && range.end <= self.bytes);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        // SAFETY: the range lies inside the view, which stays mapped while `self` lives.
        unsafe {
            VirtualProtect(
                self.view.add(range.start as usize).cast(),
                (range.end - range.start) as usize,
                protection,
                &mut old,
            )
        }
        .map_err(os)
    }

    /// Makes pages readable and writable. They read as zero: a block starts as zeros, and
    /// [`Block::close`] leaves zeros behind.
    pub fn open(&self, range: Range<u64>) -> Result<(), MemError> {
        self.protect(range, PAGE_READWRITE)
    }

    /// Gives the pages' RAM back to Windows at once and makes them inaccessible
    /// (decision D10). Whatever they held is gone; they read as zero when opened again.
    pub fn close(&self, range: Range<u64>) -> Result<(), MemError> {
        let length = (range.end - range.start) as usize;
        if length > 0 {
            assert!(range.end <= self.bytes);
            // The pages may be write-protected by a guard that has just ended, and may never
            // have been opened at all; discarding and zeroing both need them writable.
            self.protect(range.clone(), PAGE_READWRITE)?;
            // SAFETY: the range lies inside the view. The caller holds the allocator's lock
            // and is taking these pages away from the title, so no one may be using them.
            unsafe {
                let start = self.view.add(range.start as usize);
                if DiscardVirtualMemory(start.cast(), length) != 0 {
                    // Windows would not discard them. Zero them instead: the RAM stays in
                    // use, but the pages still read as zero when opened again.
                    std::ptr::write_bytes(start, 0, length);
                }
            }
        }
        self.protect(range, PAGE_NOACCESS)
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: the view and the handle came from MapViewOfFile and CreateFileMappingW and
        // are released exactly once, here. A D3D12 heap opened on the section keeps the
        // memory itself alive for as long as the heap lives.
        unsafe {
            if !self.view.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view.cast(),
                });
            }
            let _ = CloseHandle(self.handle);
        }
    }
}
