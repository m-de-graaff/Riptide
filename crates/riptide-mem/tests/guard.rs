//! Write guards on title memory, with real faults (decisions D3, D4 and M7).

use riptide_mem::{Access, BANK_BYTES, FRAME, Limits, MemError, PAGE, TitleMemory};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const ANYWHERE: Limits = Limits {
    lowest: 0x1_0000_0000,
    highest: (1 << 40) - 1,
    alignment: 0,
};

/// Title memory with `frames` frames committed, and where they start.
fn committed(frames: u64) -> (TitleMemory, u64) {
    let memory = TitleMemory::new(BANK_BYTES);
    let at = memory
        .commit(None, frames * FRAME, ANYWHERE, Access::ReadWrite)
        .unwrap();
    (memory, at)
}

fn write(address: u64, value: u8) {
    // SAFETY: every caller passes an address inside memory it committed or mapped.
    unsafe { (address as *mut u8).write_volatile(value) }
}

fn read(address: u64) -> u8 {
    // SAFETY: as above.
    unsafe { (address as *const u8).read_volatile() }
}

#[test]
fn the_first_write_is_reported_once_and_completes() {
    let (memory, at) = committed(4);
    memory.guard(at, 3 * PAGE, 7).unwrap();
    assert_eq!(memory.guard_count(), 1);
    assert!(memory.take_written().is_empty());

    write(at + PAGE + 5, 0xAB);
    assert_eq!(read(at + PAGE + 5), 0xAB);
    assert_eq!(memory.take_written(), [7]);
    assert_eq!(memory.guard_count(), 0);

    // Later writes cost nothing and report nothing.
    write(at, 1);
    write(at + 2 * PAGE, 2);
    assert!(memory.take_written().is_empty());
}

#[test]
fn reading_a_guarded_resource_does_not_end_the_guard() {
    let (memory, at) = committed(1);
    write(at + 10, 3);
    memory.guard(at, PAGE, 1).unwrap();
    assert_eq!(read(at + 10), 3);
    assert_eq!(memory.guard_count(), 1);
    assert!(memory.take_written().is_empty());
}

#[test]
fn a_write_to_one_resource_leaves_its_neighbour_guarded() {
    let (memory, at) = committed(1);
    memory.guard(at, PAGE, 1).unwrap();
    memory.guard(at + PAGE, PAGE, 2).unwrap();
    write(at + PAGE, 9);
    assert_eq!(memory.take_written(), [2]);
    assert_eq!(memory.guard_count(), 1);
    write(at, 9);
    assert_eq!(memory.take_written(), [1]);
}

#[test]
fn two_resources_on_one_page_are_both_reported_by_a_write_to_it() {
    let (memory, at) = committed(1);
    memory.guard(at, 100, 1).unwrap();
    memory.guard(at + 200, 100, 2).unwrap();
    write(at + 1000, 1);
    let mut written = memory.take_written();
    written.sort_unstable();
    assert_eq!(written, [1, 2]);
}

#[test]
fn a_write_through_another_address_of_the_same_pages_ends_the_guard() {
    let memory = TitleMemory::new(BANK_BYTES);
    let range = memory.reserve(16 * FRAME, ANYWHERE).unwrap();
    let frames = memory.allocate_frames(2).unwrap();
    let (first, second) = (range, range + 8 * FRAME);
    memory
        .map_frames(first, &frames, Access::ReadWrite)
        .unwrap();
    memory
        .map_frames(second, &frames, Access::ReadWrite)
        .unwrap();

    memory.guard(first + FRAME, PAGE, 5).unwrap();
    // The write goes in at the other address; it is the same memory.
    write(second + FRAME + 3, 0x77);
    assert_eq!(memory.take_written(), [5]);
    assert_eq!(read(first + FRAME + 3), 0x77);

    // And the first address is writable again without another report.
    write(first + FRAME + 4, 1);
    assert!(memory.take_written().is_empty());
}

#[test]
fn ending_a_guard_by_hand_reports_nothing() {
    let (memory, at) = committed(1);
    memory.guard(at, PAGE, 4).unwrap();
    assert!(memory.unguard(4));
    assert!(!memory.unguard(4));
    write(at, 1);
    assert!(memory.take_written().is_empty());
}

#[test]
fn a_guard_needs_mapped_memory_and_a_resource_not_guarded_already() {
    let (memory, at) = committed(1);
    let empty = memory.reserve(FRAME, ANYWHERE).unwrap();
    assert_eq!(memory.guard(empty, PAGE, 1), Err(MemError::InvalidAddress));
    assert_eq!(memory.guard(at, 0, 1), Err(MemError::InvalidParameter));
    memory.guard(at, PAGE, 1).unwrap();
    assert_eq!(
        memory.guard(at + PAGE, PAGE, 1),
        Err(MemError::InvalidParameter)
    );
}

#[test]
fn unmapping_guarded_pages_ends_the_guard_and_reports_it() {
    let (memory, at) = committed(2);
    memory.guard(at + FRAME, PAGE, 8).unwrap();
    memory.unmap(at + FRAME, FRAME).unwrap();
    assert_eq!(memory.take_written(), [8]);
    assert_eq!(memory.guard_count(), 0);
}

#[test]
fn guards_of_two_title_memories_do_not_mix() {
    let (one, at_one) = committed(1);
    let (two, at_two) = committed(1);
    one.guard(at_one, PAGE, 1).unwrap();
    two.guard(at_two, PAGE, 1).unwrap();
    write(at_two, 5);
    assert!(one.take_written().is_empty());
    assert_eq!(two.take_written(), [1]);
    assert_eq!(one.guard_count(), 1);
}

#[test]
fn many_threads_writing_one_guarded_resource_give_one_report_and_lose_no_write() {
    let (memory, at) = committed(1);
    let memory = Arc::new(memory);
    for round in 0..50u64 {
        memory.guard(at, 8 * PAGE, round).unwrap();
        let threads: Vec<_> = (0..8u64)
            .map(|thread| {
                std::thread::spawn(move || {
                    write(at + thread * PAGE + thread, thread as u8 + 1);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        for thread in 0..8u64 {
            assert_eq!(read(at + thread * PAGE + thread), thread as u8 + 1);
            write(at + thread * PAGE + thread, 0);
        }
        assert_eq!(memory.take_written(), [round]);
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AddVectoredExceptionHandler(
        first: u32,
        handler: unsafe extern "system" fn(*mut core::ffi::c_void) -> i32,
    ) -> *mut core::ffi::c_void;
    fn RemoveVectoredExceptionHandler(handle: *mut core::ffi::c_void) -> u32;
}

/// Access violations a handler registered ahead of the guard's has seen.
static SEEN_AHEAD: AtomicU8 = AtomicU8::new(0);

unsafe extern "system" fn count_and_pass_on(pointers: *mut core::ffi::c_void) -> i32 {
    // SAFETY: Windows passes exception pointers; the record's code is its first field.
    let code = unsafe { **pointers.cast::<*const u32>() };
    if code == 0xC000_0005 {
        SEEN_AHEAD.fetch_add(1, Ordering::SeqCst);
    }
    0
}

#[test]
fn a_handler_ahead_of_the_guard_that_passes_faults_on_does_not_break_it() {
    let (memory, at) = committed(1);
    // The guard's handler is installed by the first guard; this one goes in front of it.
    memory.guard(at, PAGE, 1).unwrap();
    assert!(memory.unguard(1));
    // SAFETY: the handler has the signature Windows calls and is removed below.
    let handle = unsafe { AddVectoredExceptionHandler(1, count_and_pass_on) };
    assert!(!handle.is_null());

    memory.guard(at, PAGE, 2).unwrap();
    let before = SEEN_AHEAD.load(Ordering::SeqCst);
    write(at, 6);
    // The handler in front saw the guarded write's fault, passed it on, and the guard
    // answered it.
    assert!(SEEN_AHEAD.load(Ordering::SeqCst) > before);
    assert_eq!(read(at), 6);
    assert_eq!(memory.take_written(), [2]);
    // SAFETY: the handle came from AddVectoredExceptionHandler above.
    unsafe { RemoveVectoredExceptionHandler(handle) };
}
