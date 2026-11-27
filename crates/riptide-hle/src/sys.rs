//! The few Windows functions this crate calls, declared here instead of pulling in a crate.

#![allow(non_snake_case)]

use std::ffi::c_void;

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_RELEASE: u32 = 0x8000;

pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;

pub const EXCEPTION_NONCONTINUABLE: u32 = 0x1;

/// Asks for a timer that fires when it says, not on the next timer tick. Windows 10 1803 on.
pub const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x2;
pub const TIMER_ALL_ACCESS: u32 = 0x1F_0003;

/// `RelationProcessorCore`: ask for one record per physical core.
pub const RELATION_PROCESSOR_CORE: i32 = 0;

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
    pub fn CreateWaitableTimerExW(
        attributes: *const c_void,
        name: *const u16,
        flags: u32,
        access: u32,
    ) -> *mut c_void;
    pub fn SetWaitableTimer(
        timer: *mut c_void,
        due_time: *const i64,
        period: i32,
        completion_routine: *const c_void,
        completion_argument: *const c_void,
        resume: i32,
    ) -> i32;
    pub fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    pub fn WaitForMultipleObjects(
        count: u32,
        handles: *const *mut c_void,
        wait_all: i32,
        milliseconds: u32,
    ) -> u32;
    pub fn CloseHandle(handle: *mut c_void) -> i32;
    pub fn Sleep(milliseconds: u32);
    pub fn GetLogicalProcessorInformationEx(
        relationship: i32,
        buffer: *mut u8,
        length: *mut u32,
    ) -> i32;
    pub fn GetCurrentThread() -> *mut c_void;
    pub fn GetCurrentProcessorNumber() -> u32;
    pub fn SetThreadAffinityMask(thread: *mut c_void, mask: usize) -> usize;
    pub fn SetThreadIdealProcessor(thread: *mut c_void, processor: u32) -> u32;
    pub fn CreateThread(
        attributes: *const c_void,
        stack_size: usize,
        start: *const c_void,
        parameter: *const c_void,
        flags: u32,
        thread_id: *mut u32,
    ) -> *mut c_void;
    pub fn ResumeThread(thread: *mut c_void) -> u32;
}
