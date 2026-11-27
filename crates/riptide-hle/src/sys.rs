//! The few Windows functions this crate calls, declared here instead of pulling in a crate.

#![allow(non_snake_case)]

use std::ffi::c_void;

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_RELEASE: u32 = 0x8000;

pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;

/// Look for the module in Windows' own folder only, never beside the program or the title.
pub const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;

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
    pub fn RtlCaptureStackBackTrace(
        frames_to_skip: u32,
        frames_to_capture: u32,
        back_trace: *mut *mut c_void,
        hash: *mut u32,
    ) -> u16;
    pub fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
    pub fn LoadLibraryExA(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void;
    pub fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    pub fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    pub fn FreeLibrary(module: *mut c_void) -> i32;
    pub fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    pub fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    pub fn GetModuleFileNameW(module: *mut c_void, buffer: *mut u16, size: u32) -> u32;
    pub fn GetModuleFileNameA(module: *mut c_void, buffer: *mut u8, size: u32) -> u32;
    pub fn GetCommandLineA() -> *const std::ffi::c_char;
    pub fn SetLastError(code: u32);
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

/// `SYSTEM_INFO`, as `GetSystemInfo` fills it.
#[repr(C)]
pub struct SYSTEM_INFO {
    pub processor_architecture: u16,
    pub reserved: u16,
    pub page_size: u32,
    pub minimum_application_address: *mut c_void,
    pub maximum_application_address: *mut c_void,
    pub active_processor_mask: usize,
    pub number_of_processors: u32,
    pub processor_type: u32,
    pub allocation_granularity: u32,
    pub processor_level: u16,
    pub processor_revision: u16,
}

impl Default for SYSTEM_INFO {
    fn default() -> Self {
        // SAFETY: every field is a number or a pointer, for which zero is a value.
        unsafe { std::mem::zeroed() }
    }
}

/// `MEM_ADDRESS_REQUIREMENTS`: where `VirtualAlloc2` may place memory.
#[repr(C)]
pub struct MEM_ADDRESS_REQUIREMENTS {
    pub lowest_starting_address: *mut c_void,
    pub highest_ending_address: *mut c_void,
    pub alignment: usize,
}

/// `MEM_EXTENDED_PARAMETER` holding a pointer: its type in the low bits of the first word.
#[repr(C)]
pub struct MEM_EXTENDED_PARAMETER {
    pub kind: u64,
    pub pointer: *mut c_void,
}

/// `MemExtendedParameterAddressRequirements`.
pub const MEM_EXTENDED_PARAMETER_ADDRESS_REQUIREMENTS: u64 = 1;

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetSystemInfo(info: *mut SYSTEM_INFO);
}

// `VirtualAlloc2` has no entry in kernel32's import library; it lives in kernelbase.
#[link(name = "kernelbase", kind = "raw-dylib")]
unsafe extern "system" {
    pub fn VirtualAlloc2(
        process: *mut c_void,
        base: *mut c_void,
        size: usize,
        allocation_type: u32,
        protection: u32,
        parameters: *mut MEM_EXTENDED_PARAMETER,
        parameter_count: u32,
    ) -> *mut c_void;
}

unsafe extern "C" {
    pub fn _aligned_malloc(size: usize, alignment: usize) -> *mut c_void;
    pub fn _aligned_free(block: *mut c_void);
    pub fn _initterm(first: *const c_void, last: *const c_void);
}
