//! Write guards: protect, catch the first write, recover.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

use riptide_mem::{MemError, PAGE, TitleMemory};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_PROTECTION_FLAGS,
    PAGE_READONLY, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
};

const MIB: u64 = 1 << 20;

fn memory() -> TitleMemory {
    TitleMemory::create(64 * MIB).unwrap()
}

/// Commits `pages` pages and fills them with a pattern.
fn committed(memory: &TitleMemory, pages: u64) -> u64 {
    let address = memory.allocate(pages * PAGE).unwrap();
    for offset in 0..pages * PAGE {
        write(address + offset, offset as u8);
    }
    address
}

fn write(address: u64, value: u8) {
    // SAFETY: each test writes only inside pages it has committed. A guarded page faults
    // and the guard makes it writable before the write is tried again.
    unsafe { (address as *mut u8).write_volatile(value) }
}

fn read(address: u64) -> u8 {
    // SAFETY: each test reads only inside pages it has committed.
    unsafe { (address as *const u8).read_volatile() }
}

/// What Windows says the protection of the page at `address` is.
fn protection(address: u64) -> PAGE_PROTECTION_FLAGS {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `info` is a valid out-pointer of the size passed.
    let filled = unsafe {
        VirtualQuery(
            Some(address as *const c_void),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    assert_ne!(filled, 0);
    info.Protect
}

#[test]
fn the_first_write_is_reported_once_and_completes() {
    let memory = memory();
    let address = committed(&memory, 4);
    memory.guard(address, 4 * PAGE, 7).unwrap();
    assert_eq!(memory.guard_count(), 1);
    assert_eq!(protection(address), PAGE_READONLY);
    assert_eq!(protection(address + 3 * PAGE), PAGE_READONLY);

    write(address + 2 * PAGE + 5, 0xAB);

    assert_eq!(read(address + 2 * PAGE + 5), 0xAB, "the write went through");
    assert_eq!(memory.take_written(), [7]);
    assert_eq!(memory.guard_count(), 0);
    // Protection is dropped for the whole resource, not only for the page written.
    for page in 0..4 {
        assert_eq!(protection(address + page * PAGE), PAGE_READWRITE);
    }

    // Later writes cost nothing and report nothing.
    write(address, 1);
    write(address + 3 * PAGE, 2);
    assert_eq!(memory.take_written(), []);
    // Nothing else on the pages changed.
    assert_eq!(read(address + 2 * PAGE + 6), 6);
}

#[test]
fn reading_a_guarded_resource_does_not_end_the_guard() {
    let memory = memory();
    let address = committed(&memory, 2);
    memory.guard(address, 2 * PAGE, 1).unwrap();
    let sum: u64 = (0..2 * PAGE).map(|o| u64::from(read(address + o))).sum();
    assert!(sum > 0);
    assert_eq!(memory.take_written(), []);
    assert_eq!(memory.guard_count(), 1);
}

#[test]
fn a_write_to_one_resource_leaves_its_neighbour_guarded() {
    let memory = memory();
    let address = committed(&memory, 4);
    memory.guard(address, 2 * PAGE, 1).unwrap();
    memory.guard(address + 2 * PAGE, 2 * PAGE, 2).unwrap();

    write(address + 10, 0xFF);

    assert_eq!(memory.take_written(), [1]);
    assert_eq!(protection(address + PAGE), PAGE_READWRITE);
    assert_eq!(protection(address + 2 * PAGE), PAGE_READONLY);
    assert_eq!(memory.guard_count(), 1);

    write(address + 3 * PAGE, 0xFF);
    assert_eq!(memory.take_written(), [2]);
}

#[test]
fn two_resources_on_one_page_are_both_reported_by_a_write_to_it() {
    let memory = memory();
    let address = committed(&memory, 1);
    memory.guard(address, 100, 1).unwrap();
    memory.guard(address + 2000, 100, 2).unwrap();

    // The write is beside both resources, but on their page.
    write(address + 1000, 0xFF);

    let mut written = memory.take_written();
    written.sort();
    assert_eq!(written, [1, 2]);
    assert_eq!(read(address + 1000), 0xFF);
}

#[test]
fn a_page_shared_with_a_live_guard_stays_protected() {
    let memory = memory();
    let address = committed(&memory, 5);
    // Resource 1 covers pages 0 to 2, resource 2 pages 2 to 4: they share page 2.
    memory.guard(address, 3 * PAGE, 1).unwrap();
    memory.guard(address + 2 * PAGE, 3 * PAGE, 2).unwrap();

    write(address, 0xFF);

    assert_eq!(memory.take_written(), [1]);
    assert_eq!(protection(address + PAGE), PAGE_READWRITE);
    assert_eq!(
        protection(address + 2 * PAGE),
        PAGE_READONLY,
        "the shared page still guards resource 2"
    );
    write(address + 2 * PAGE, 0xFF);
    assert_eq!(memory.take_written(), [2]);
    assert_eq!(protection(address + 4 * PAGE), PAGE_READWRITE);
}

#[test]
fn ending_a_guard_by_hand_reports_nothing() {
    let memory = memory();
    let address = committed(&memory, 2);
    memory.guard(address, 2 * PAGE, 9).unwrap();

    assert!(memory.unguard(9));
    assert_eq!(protection(address), PAGE_READWRITE);
    write(address, 0xFF);
    assert_eq!(memory.take_written(), []);
    assert!(!memory.unguard(9), "it is not guarded any more");
}

#[test]
fn a_guard_needs_committed_pages_and_a_resource_not_guarded_already() {
    let memory = memory();
    let reserved = memory.reserve(None, 4 * PAGE).unwrap();
    assert_eq!(
        memory.guard(reserved, PAGE, 1),
        Err(MemError::InvalidAddress)
    );
    memory.commit(reserved, PAGE).unwrap();
    assert_eq!(memory.guard(reserved, PAGE, 1), Ok(()));
    // The second page is still only reserved.
    assert_eq!(
        memory.guard(reserved, 2 * PAGE, 2),
        Err(MemError::InvalidAddress)
    );
    assert_eq!(
        memory.guard(reserved, PAGE, 1),
        Err(MemError::InvalidParameter)
    );
    assert_eq!(
        memory.guard(reserved, 0, 3),
        Err(MemError::InvalidParameter)
    );
    assert_eq!(memory.guard(0x1000, PAGE, 3), Err(MemError::InvalidAddress));
    assert_eq!(memory.guard_count(), 1);
}

#[test]
fn decommitting_guarded_pages_ends_the_guard_and_reports_it() {
    let memory = memory();
    let address = committed(&memory, 4);
    memory.guard(address, 2 * PAGE, 1).unwrap();
    memory.guard(address + 2 * PAGE, 2 * PAGE, 2).unwrap();

    memory.decommit(address, PAGE).unwrap();

    assert_eq!(memory.take_written(), [1]);
    assert_eq!(memory.guard_count(), 1);
    memory.release(address).unwrap();
    assert_eq!(memory.take_written(), [2]);
    assert_eq!(memory.guard_count(), 0);
}

#[test]
fn guards_of_two_title_memories_do_not_mix() {
    let (first, second) = (memory(), memory());
    let a = committed(&first, 1);
    let b = committed(&second, 1);
    first.guard(a, PAGE, 5).unwrap();
    second.guard(b, PAGE, 5).unwrap();

    write(a, 0xFF);

    assert_eq!(first.take_written(), [5]);
    assert_eq!(second.take_written(), []);
    assert_eq!(second.guard_count(), 1);
}

#[test]
fn many_threads_writing_one_guarded_resource_give_one_report_and_lose_no_write() {
    const THREADS: u64 = 8;
    let memory = memory();
    let address = committed(&memory, THREADS);
    for round in 0..50u64 {
        memory.guard(address, THREADS * PAGE, round).unwrap();
        let start = Arc::new(Barrier::new(THREADS as usize));
        let threads: Vec<_> = (0..THREADS)
            .map(|thread| {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    // Half the threads hit the same page, so some fault on a page whose
                    // guard another thread has just ended.
                    let page = if thread % 2 == 0 { 0 } else { thread };
                    write(address + page * PAGE + thread, round as u8 ^ 0x5A);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(memory.take_written(), [round], "round {round}");
        for thread in 0..THREADS {
            let page = if thread % 2 == 0 { 0 } else { thread };
            assert_eq!(read(address + page * PAGE + thread), round as u8 ^ 0x5A);
        }
    }
}

/// A page of the "title's own", outside title memory, and how often its handler ran.
static TITLE_PAGE: AtomicUsize = AtomicUsize::new(0);
static TITLE_HANDLER_RUNS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut c_void,
    parameter_count: u32,
    parameters: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut c_void,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AddVectoredExceptionHandler(
        first: u32,
        handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32,
    ) -> *mut c_void;
    fn RemoveVectoredExceptionHandler(handle: *mut c_void) -> u32;
}

/// Stands in for a title's own exception handler: it deals with write faults on its own
/// read-only page by making the page writable.
unsafe extern "system" fn title_handler(pointers: *mut ExceptionPointers) -> i32 {
    // SAFETY: Windows passes valid exception pointers to a vectored handler.
    let record = unsafe { &*(*pointers).record };
    let page = TITLE_PAGE.load(Ordering::SeqCst);
    if record.code == 0xC000_0005
        && record.parameters[0] == 1
        && (page..page + PAGE as usize).contains(&record.parameters[1])
    {
        TITLE_HANDLER_RUNS.fetch_add(1, Ordering::SeqCst);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        // SAFETY: the page is the one this test allocated.
        unsafe {
            VirtualProtect(
                page as *const c_void,
                PAGE as usize,
                PAGE_READWRITE,
                &mut old,
            )
        }
        .unwrap();
        return -1; // EXCEPTION_CONTINUE_EXECUTION
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

#[test]
fn a_title_handler_for_other_faults_still_runs() {
    let memory = memory();
    let address = committed(&memory, 1);
    // The guard's handler is installed with the first guard, ahead of every other handler.
    memory.guard(address, PAGE, 1).unwrap();

    // SAFETY: a fresh read-only page of this test's own, freed at the end.
    let page = unsafe { VirtualAlloc(None, PAGE as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READONLY) }
        as usize;
    assert_ne!(page, 0);
    TITLE_PAGE.store(page, Ordering::SeqCst);
    // SAFETY: `title_handler` has the signature Windows calls; it is removed below.
    let handle = unsafe { AddVectoredExceptionHandler(0, title_handler) };

    // A fault that is not the guard's: it is passed on, and the title's handler deals
    // with it.
    write(page as u64 + 8, 0x77);
    assert_eq!(read(page as u64 + 8), 0x77);
    assert_eq!(TITLE_HANDLER_RUNS.load(Ordering::SeqCst), 1);
    assert_eq!(memory.take_written(), [], "the guard was not touched");
    assert_eq!(memory.guard_count(), 1);

    // And the guard's own fault never reaches the title's handler.
    write(address, 0x11);
    assert_eq!(memory.take_written(), [1]);
    assert_eq!(TITLE_HANDLER_RUNS.load(Ordering::SeqCst), 1);

    // SAFETY: the handle and the page came from the calls above and are released once.
    unsafe {
        RemoveVectoredExceptionHandler(handle);
        VirtualFree(page as *mut c_void, 0, MEM_RELEASE).unwrap();
    }
}

#[test]
fn what_a_guard_and_its_fault_cost_on_this_machine() {
    const ROUNDS: u32 = 2000;
    let memory = memory();
    for pages in [1u64, 16, 256] {
        let address = committed(&memory, pages);
        let (mut guarding, mut faulting) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        for round in 0..ROUNDS {
            let start = Instant::now();
            memory
                .guard(address, pages * PAGE, u64::from(round))
                .unwrap();
            guarding += start.elapsed();
            let start = Instant::now();
            write(address, round as u8);
            faulting += start.elapsed();
        }
        assert_eq!(memory.take_written().len(), ROUNDS as usize);
        println!(
            "{pages:>3} pages: guard {:?}, first write {:?}",
            guarding / ROUNDS,
            faulting / ROUNDS
        );
        memory.release(address).unwrap();
    }
}
