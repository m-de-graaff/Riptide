//! The few Windows functions the loader calls, declared here so the crate has no dependencies.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_RELEASE: u32 = 0x8000;

pub const PAGE_READONLY: u32 = 0x02;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// Returned by an unhandled-exception filter to end the process without an error dialog.
pub const EXCEPTION_EXECUTE_HANDLER: i32 = 1;

pub const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const EXCEPTION_IN_PAGE_ERROR: u32 = 0xC000_0006;
pub const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const EXCEPTION_PRIV_INSTRUCTION: u32 = 0xC000_0096;
pub const EXCEPTION_INT_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub const EXCEPTION_STACK_OVERFLOW: u32 = 0xC000_00FD;
pub const EXCEPTION_BREAKPOINT: u32 = 0x8000_0003;

/// `EXCEPTION_RECORD` for x64.
#[repr(C)]
pub struct EXCEPTION_RECORD {
    pub ExceptionCode: u32,
    pub ExceptionFlags: u32,
    pub ExceptionRecord: *mut EXCEPTION_RECORD,
    pub ExceptionAddress: *mut c_void,
    pub NumberParameters: u32,
    pub ExceptionInformation: [usize; 15],
}

#[repr(C)]
pub struct EXCEPTION_POINTERS {
    pub ExceptionRecord: *mut EXCEPTION_RECORD,
    pub ContextRecord: *mut c_void,
}

pub type ExceptionFilter = unsafe extern "system" fn(*mut EXCEPTION_POINTERS) -> i32;

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn VirtualAlloc(
        address: *mut c_void,
        size: usize,
        allocation_type: u32,
        protection: u32,
    ) -> *mut c_void;
    pub fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    pub fn VirtualProtect(
        address: *mut c_void,
        size: usize,
        protection: u32,
        old_protection: *mut u32,
    ) -> i32;
    pub fn GetCurrentProcess() -> *mut c_void;
    pub fn GetCurrentThreadId() -> u32;
    pub fn CreateThread(
        attributes: *const c_void,
        stack_size: usize,
        start: unsafe extern "system" fn(*mut c_void) -> u32,
        parameter: *mut c_void,
        flags: u32,
        thread_id: *mut u32,
    ) -> *mut c_void;
    pub fn ResumeThread(thread: *mut c_void) -> u32;
    pub fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    pub fn GetExitCodeThread(thread: *mut c_void, exit_code: *mut u32) -> i32;
    pub fn CloseHandle(handle: *mut c_void) -> i32;
    pub fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
    pub fn GetModuleFileNameW(module: *mut c_void, buffer: *mut u16, size: u32) -> u32;
    pub fn VirtualQuery(address: *const c_void, info: *mut c_void, length: usize) -> usize;
    pub fn LoadLibraryW(name: *const u16) -> *mut c_void;
    pub fn FreeLibrary(module: *mut c_void) -> i32;
    pub fn FlushInstructionCache(process: *mut c_void, address: *const c_void, size: usize) -> i32;
    pub fn RtlAddFunctionTable(table: *mut c_void, entry_count: u32, base: u64) -> u8;
    pub fn RtlDeleteFunctionTable(table: *const c_void) -> u8;
    pub fn SetUnhandledExceptionFilter(filter: Option<ExceptionFilter>) -> Option<ExceptionFilter>;
}
