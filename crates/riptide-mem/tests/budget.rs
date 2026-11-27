//! Use held against the RAM budget, and freed pages going back to Windows.

use riptide_mem::{PAGE, Pressure, RamBudget, TitleMemory};
use riptide_trace::Ledger;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;

const MIB: u64 = 1 << 20;

/// The working set belongs to the whole process, and tests run side by side in it. The
/// tests that read it take turns, so one test's memory is not in another's reading.
static WORKING_SET: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn alone() -> std::sync::MutexGuard<'static, ()> {
    WORKING_SET
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// The RAM this process has in use right now, in bytes: its working set. Title memory is a
/// shared section, so it shows here and not in Windows' "private bytes".
fn working_set() -> u64 {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: `counters` is a valid out-pointer of the size passed.
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    }
    .unwrap();
    counters.WorkingSetSize as u64
}

/// Writes one byte to every page, so each takes RAM.
fn touch(address: u64, bytes: u64) {
    for offset in (0..bytes).step_by(PAGE as usize) {
        // SAFETY: the caller committed the range.
        unsafe { ((address + offset) as *mut u8).write_volatile(0x5A) };
    }
}

fn read(address: u64) -> u8 {
    // SAFETY: the caller committed the page.
    unsafe { (address as *const u8).read_volatile() }
}

#[test]
fn freeing_a_range_gives_its_ram_back_to_windows_at_once() {
    const SIZE: u64 = 256 * MIB;
    let _alone = alone();
    let memory = TitleMemory::create(512 * MIB).unwrap();
    let address = memory.allocate(SIZE).unwrap();
    let before = working_set();

    touch(address, SIZE);
    let touched = working_set();
    assert!(
        touched + 16 * MIB >= before + SIZE,
        "touching 256 MiB took {} MiB",
        touched.saturating_sub(before) / MIB
    );

    memory.decommit(address, SIZE).unwrap();
    let freed = working_set();
    assert!(
        freed <= before + 16 * MIB,
        "after freeing, {} MiB more than before is still in use",
        freed.saturating_sub(before) / MIB
    );
    println!(
        "working set: {} MiB before, {} MiB touched, {} MiB after freeing",
        before / MIB,
        touched / MIB,
        freed / MIB
    );
}

#[test]
fn freeing_part_of_a_range_gives_back_that_part_and_keeps_the_rest() {
    const SIZE: u64 = 128 * MIB;
    let _alone = alone();
    let memory = TitleMemory::create(256 * MIB).unwrap();
    let address = memory.allocate(SIZE).unwrap();
    touch(address, SIZE);
    let touched = working_set();

    memory.decommit(address + SIZE / 2, SIZE / 2).unwrap();

    let after = working_set();
    assert!(
        after + SIZE / 2 <= touched + 16 * MIB,
        "half was freed, yet only {} MiB came back",
        touched.saturating_sub(after) / MIB
    );
    assert_eq!(
        read(address),
        0x5A,
        "the half still committed keeps its bytes"
    );
    assert_eq!(read(address + SIZE / 2 - PAGE), 0x5A);
    assert_eq!(memory.committed_bytes(), SIZE / 2);
}

#[test]
fn pages_committed_again_read_as_zero() {
    let memory = TitleMemory::create(64 * MIB).unwrap();
    let address = memory.allocate(4 * PAGE).unwrap();
    touch(address, 4 * PAGE);
    assert_eq!(read(address + PAGE), 0x5A);

    memory.decommit(address, 4 * PAGE).unwrap();
    memory.commit(address, 4 * PAGE).unwrap();

    for page in 0..4 {
        assert_eq!(read(address + page * PAGE), 0);
    }
}

#[test]
fn guarded_pages_are_given_back_too() {
    // A guard leaves its pages read-only. Freeing them must still work and report the
    // guard as ended.
    let memory = TitleMemory::create(64 * MIB).unwrap();
    let address = memory.allocate(2 * PAGE).unwrap();
    touch(address, 2 * PAGE);
    memory.guard(address, 2 * PAGE, 3).unwrap();

    memory.decommit(address, 2 * PAGE).unwrap();

    assert_eq!(memory.take_written(), [3]);
    memory.commit(address, 2 * PAGE).unwrap();
    assert_eq!(read(address), 0);
}

#[test]
fn committed_title_memory_is_counted_as_it_changes() {
    let memory = TitleMemory::create(64 * MIB).unwrap();
    assert_eq!(memory.committed_bytes(), 0);

    let reserved = memory.reserve(None, MIB).unwrap();
    assert_eq!(memory.committed_bytes(), 0, "a reservation uses nothing");
    memory.commit(reserved, 10 * PAGE).unwrap();
    assert_eq!(memory.committed_bytes(), 10 * PAGE);
    // Committing pages that are committed already adds nothing.
    memory.commit(reserved + 5 * PAGE, 10 * PAGE).unwrap();
    assert_eq!(memory.committed_bytes(), 15 * PAGE);
    memory.decommit(reserved, 3 * PAGE).unwrap();
    assert_eq!(memory.committed_bytes(), 12 * PAGE);

    let other = memory.allocate(2 * MIB).unwrap();
    assert_eq!(memory.committed_bytes(), 12 * PAGE + 2 * MIB);
    memory.release(reserved).unwrap();
    assert_eq!(memory.committed_bytes(), 2 * MIB);
    memory.release(other).unwrap();
    assert_eq!(memory.committed_bytes(), 0);
}

#[test]
fn use_is_held_against_the_budget() {
    let memory = TitleMemory::create(64 * MIB).unwrap();
    let idle = memory.ram_use();
    assert_eq!(idle.title, 0);
    assert!(idle.private > 0);

    // A budget with room for this process and 32 MiB of title memory.
    let limit_mib = (idle.private / MIB + 64) as u32;
    let budget = RamBudget::new(Some(limit_mib), 0);
    assert_eq!(budget.pressure(memory.ram_use()), Pressure::Under);

    let address = memory.allocate(60 * MIB).unwrap();
    assert_eq!(memory.ram_use().title, 60 * MIB);
    assert!(
        budget.pressure(memory.ram_use()) >= Pressure::Near,
        "60 of 64 MiB of room is used"
    );

    memory.release(address).unwrap();
    assert_eq!(budget.pressure(memory.ram_use()), Pressure::Under);
}

#[test]
fn use_is_reported_to_the_trace_ledger() {
    let memory = TitleMemory::create(64 * MIB).unwrap();
    let ledger = Ledger::new();

    let before = ledger.end_frame(memory.sample(0)).frame;
    let address = memory.allocate(32 * MIB).unwrap();
    let during = ledger.end_frame(memory.sample(7 * MIB)).frame;
    memory.release(address).unwrap();
    let after = ledger.end_frame(memory.sample(0)).frame;

    // The process's own memory moves a little between frames; title memory moves by
    // exactly what was committed.
    let slack = 8 * MIB;
    assert!(before.private_bytes > 0);
    assert!(during.private_bytes >= before.private_bytes + 32 * MIB - slack);
    assert!(after.private_bytes <= during.private_bytes - 32 * MIB + slack);
    assert_eq!(during.video_bytes, 7 * MIB);
}
