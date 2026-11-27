//! The few Windows functions this crate calls, declared here instead of pulling in a crate.

#![allow(non_snake_case)]

use std::ffi::c_void;

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_RELEASE: u32 = 0x8000;

pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;

pub const EXCEPTION_NONCONTINUABLE: u32 = 0x1;

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
    pub fn FlushInstructionCache(process: *mut c_void, address: *const c_void, size: usize) -> i32;
    pub fn RaiseException(code: u32, flags: u32, argument_count: u32, arguments: *const usize);
    pub fn LoadLibraryA(name: *const u8) -> *mut c_void;
    pub fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}
