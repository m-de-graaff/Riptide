//! The stand-in title: a small freestanding PE32+ image of our own that the loader tests
//! load, bind and run in place of a real title.
//!
//! It has what the loader has to handle and nothing else: imports by name and by ordinal from
//! two modules, a pointer that needs a base relocation, writable data, a TLS callback, and an
//! entry point that returns an exit code. It reports everything it sees through
//! `StandInReport`, so a test can check each value.
//!
//! The module and function names are made up for this fixture. No console SDK is involved.

#![no_std]
#![no_main]

use core::ffi::c_void;
use core::hint::black_box;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};

/// What the entry point returns when all went well.
const EXIT_CODE: u32 = 42;

// Report ids. `riptide-pe/tests/stand_in.rs` holds the same numbers.
const REPORT_TLS_CALLBACK: u32 = 1;
const REPORT_ENTERED: u32 = 2;
const REPORT_ADD: u32 = 3;
const REPORT_ORDINAL: u32 = 4;
const REPORT_RELOCATED: u32 = 5;
const REPORT_COUNTER: u32 = 6;
const REPORT_THREAD: u32 = 7;
const REPORT_SLEPT: u32 = 8;

/// `StandInMode` answers this to ask for a write to address zero.
const MODE_FAULT: u32 = 1;
/// `StandInMode` answers this to ask for a call to `StandInUnbuilt`.
const MODE_UNBUILT: u32 = 2;
/// `StandInMode` answers this to ask for [`SLEEPS`] sleeps of one millisecond.
const MODE_SLEEP: u32 = 3;

/// How many times [`MODE_SLEEP`] sleeps.
const SLEEPS: u32 = 1000;

#[link(name = "standin_console", kind = "raw-dylib")]
unsafe extern "C" {
    /// Hands one observed value to the test.
    safe fn StandInReport(id: u32, value: u64);
    /// Returns `a + b`; shows that arguments and results cross the import correctly.
    safe fn StandInAdd(a: u64, b: u64) -> u64;
    /// What the test wants this run to do: 0 to run to the exit, [`MODE_FAULT`] to fault,
    /// [`MODE_UNBUILT`] to call `StandInUnbuilt`, [`MODE_SLEEP`] to sleep 1 ms a thousand times.
    safe fn StandInMode() -> u32;
    /// A function no test implements: what a title's call to an unbuilt console function
    /// looks like. Only called in [`MODE_UNBUILT`].
    safe fn StandInUnbuilt() -> u64;
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// A function whose console version is the host's own, to be passed straight through.
    safe fn GetCurrentThreadId() -> u32;
    /// The title's sleep. On the console a 1 ms sleep takes 1 ms.
    safe fn Sleep(milliseconds: u32);
}

#[link(name = "standin_ordinal", kind = "raw-dylib")]
unsafe extern "C" {
    /// Imported by ordinal 7, with no name in the image.
    #[link_ordinal(7)]
    safe fn StandInByOrdinal() -> u64;
}

/// A value reached through [`POINTER`].
static VALUE: u64 = 0x5EED_0000_CAFE;

/// A pointer stored in the image. Its bytes hold an absolute address, so the loader has to
/// relocate it when the image is not mapped at its preferred base.
static POINTER: &u64 = &VALUE;

/// Writable data: wrong section protection would fault here.
static COUNTER: AtomicU64 = AtomicU64::new(100);

type TlsCallback =
    unsafe extern "system" fn(module: *mut c_void, reason: u32, reserved: *mut c_void);

/// The image's TLS directory, laid out as `IMAGE_TLS_DIRECTORY64`.
#[repr(C)]
pub struct TlsDirectory {
    start_of_raw_data: *const u8,
    end_of_raw_data: *const u8,
    address_of_index: *const u32,
    address_of_callbacks: *const Option<TlsCallback>,
    size_of_zero_fill: u32,
    characteristics: u32,
}

// SAFETY: the directory is read-only data that nothing in this image writes.
unsafe impl Sync for TlsDirectory {}

static TLS_INDEX: u32 = 0;

/// The callbacks the loader runs before the entry point, ended by a null entry.
static TLS_CALLBACKS: [Option<TlsCallback>; 2] = [Some(tls_callback), None];

/// The linker makes the symbol `_tls_used` the image's TLS directory. The C runtime normally
/// defines it; this image has no C runtime. There is no thread-local data, only a callback.
#[unsafe(no_mangle)]
#[used]
pub static _tls_used: TlsDirectory = TlsDirectory {
    start_of_raw_data: core::ptr::null(),
    end_of_raw_data: core::ptr::null(),
    address_of_index: &raw const TLS_INDEX,
    address_of_callbacks: TLS_CALLBACKS.as_ptr(),
    size_of_zero_fill: 0,
    characteristics: 0,
};

unsafe extern "system" fn tls_callback(_module: *mut c_void, reason: u32, _reserved: *mut c_void) {
    StandInReport(REPORT_TLS_CALLBACK, u64::from(reason));
}

/// The image's entry point.
#[unsafe(no_mangle)]
pub extern "C" fn title_entry() -> u32 {
    StandInReport(REPORT_ENTERED, 0);
    StandInReport(REPORT_ADD, StandInAdd(40, 2));
    StandInReport(REPORT_ORDINAL, StandInByOrdinal());
    StandInReport(REPORT_RELOCATED, **black_box(&POINTER));
    StandInReport(REPORT_COUNTER, COUNTER.fetch_add(1, Ordering::Relaxed) + 1);

    StandInReport(REPORT_THREAD, u64::from(GetCurrentThreadId()));

    let mode = StandInMode();
    if mode == MODE_UNBUILT {
        StandInReport(REPORT_ENTERED, StandInUnbuilt());
    }
    if mode == MODE_SLEEP {
        let mut slept = 0;
        while slept < SLEEPS {
            Sleep(1);
            slept += 1;
        }
        StandInReport(REPORT_SLEPT, u64::from(slept));
    }
    if mode == MODE_FAULT {
        let nowhere = black_box(0usize) as *mut u32;
        // SAFETY: none. This write faults on purpose, so a test can check that the loader
        // names the faulting address as module plus offset.
        unsafe { nowhere.write_volatile(0xDEAD) };
    }
    EXIT_CODE
}

// What the C runtime would supply. The core library refers to these symbols, so a freestanding
// image has to define them, used or not.

/// Copies `count` bytes; the regions do not overlap.
///
/// # Safety
/// `dest` and `src` are valid for `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    let mut i = 0;
    while i < count {
        // SAFETY: the caller guarantees both regions hold `count` bytes.
        unsafe { dest.add(i).write(src.add(i).read()) };
        i += 1;
    }
    dest
}

/// Fills `count` bytes with `value`.
///
/// # Safety
/// `dest` is valid for `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, value: i32, count: usize) -> *mut u8 {
    let mut i = 0;
    while i < count {
        // SAFETY: the caller guarantees the region holds `count` bytes.
        unsafe { dest.add(i).write(value as u8) };
        i += 1;
    }
    dest
}

/// Compares `count` bytes.
///
/// # Safety
/// `a` and `b` are valid for `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, count: usize) -> i32 {
    let mut i = 0;
    while i < count {
        // SAFETY: the caller guarantees both regions hold `count` bytes.
        let (x, y) = unsafe { (a.add(i).read(), b.add(i).read()) };
        if x != y {
            return i32::from(x) - i32::from(y);
        }
        i += 1;
    }
    0
}

/// The linker asks for this symbol in any image that uses floating point.
#[unsafe(no_mangle)]
#[used]
pub static _fltused: i32 = 0;

/// The C++ exception handler the core library's unwind tables name. Nothing unwinds here:
/// panics abort.
#[unsafe(no_mangle)]
pub extern "C" fn __CxxFrameHandler3() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
