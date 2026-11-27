//! Loads and runs the stand-in title: the image in `tests/fixtures/stand-in-title`, built
//! from our own source when these tests run.

use std::cell::{Cell, RefCell};
use std::process::Command;

use riptide_pe::{Image, LoadError, Symbol, describe, load, report_unhandled_faults};

const NAME: &str = "stand-in-title.exe";

/// What the stand-in title's entry point returns.
const EXIT_CODE: u32 = 42;

// Report ids. The stand-in title's source holds the same numbers.
const REPORT_TLS_CALLBACK: u32 = 1;
const REPORT_ENTERED: u32 = 2;
const REPORT_ADD: u32 = 3;
const REPORT_ORDINAL: u32 = 4;
const REPORT_RELOCATED: u32 = 5;
const REPORT_COUNTER: u32 = 6;
const REPORT_THREAD: u32 = 7;

const MODE_FAULT: u32 = 1;

/// Set in the child process that the fault test starts.
const FAULT_CHILD: &str = "RIPTIDE_PE_FAULT_CHILD";

/// The stand-in title's image, built from source on first use.
fn stand_in() -> &'static [u8] {
    riptide_testkit::stand_in_title_bytes()
}

// The functions the stand-in title imports. The title runs on the thread that called `run`,
// so what it reports is kept per thread and tests can run side by side.
thread_local! {
    static REPORTS: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
    static MODE: Cell<u32> = const { Cell::new(0) };
}

extern "C" fn stand_in_report(id: u32, value: u64) {
    REPORTS.with_borrow_mut(|reports| reports.push((id, value)));
}

extern "C" fn stand_in_add(a: u64, b: u64) -> u64 {
    a + b
}

extern "C" fn stand_in_mode() -> u32 {
    MODE.get()
}

extern "C" fn stand_in_by_ordinal() -> u64 {
    0x0D1A
}

/// The title only calls this when asked to; these tests never ask.
extern "C" fn stand_in_unbuilt() -> u64 {
    unreachable!("no loader test runs the stand-in title in that mode")
}

#[link(name = "kernel32")]
unsafe extern "system" {
    safe fn GetCurrentThreadId() -> u32;
    fn GetModuleHandleA(name: *const u8) -> *mut std::ffi::c_void;
    fn GetProcAddress(module: *mut std::ffi::c_void, name: *const u8) -> *mut std::ffi::c_void;
}

/// The host's own kernel32 function of that name. The stand-in title imports a few, and
/// these tests give it the real ones.
fn kernel32(name: &str) -> Option<usize> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: both names are null-terminated, and kernel32 is loaded in every process.
    let address = unsafe {
        GetProcAddress(
            GetModuleHandleA(c"kernel32.dll".as_ptr().cast()),
            name.as_ptr().cast(),
        )
    };
    (!address.is_null()).then_some(address as usize)
}

/// Resolves every import of the stand-in title.
fn resolver(module: &str, symbol: &Symbol) -> Option<usize> {
    match (module, symbol) {
        ("standin_console.dll", Symbol::Name(name)) => match name.as_str() {
            "StandInReport" => Some(stand_in_report as *const () as usize),
            "StandInAdd" => Some(stand_in_add as *const () as usize),
            "StandInMode" => Some(stand_in_mode as *const () as usize),
            "StandInUnbuilt" => Some(stand_in_unbuilt as *const () as usize),
            _ => None,
        },
        ("standin_ordinal.dll", Symbol::Ordinal(7)) => {
            Some(stand_in_by_ordinal as *const () as usize)
        }
        ("kernel32.dll", Symbol::Name(name)) => kernel32(name),
        _ => None,
    }
}

fn take_reports() -> Vec<(u32, u64)> {
    REPORTS.take()
}

/// What a complete run reports, in order.
fn expected_reports() -> Vec<(u32, u64)> {
    vec![
        (REPORT_TLS_CALLBACK, 1),
        (REPORT_ENTERED, 0),
        (REPORT_ADD, 42),
        (REPORT_ORDINAL, 0x0D1A),
        (REPORT_RELOCATED, 0x5EED_0000_CAFE),
        (REPORT_COUNTER, 101),
        (REPORT_THREAD, u64::from(GetCurrentThreadId())),
    ]
}

#[test]
fn the_image_parses_with_its_imports_sections_and_tls() {
    let image = Image::parse(stand_in()).unwrap();

    let imports: Vec<String> = image
        .imports
        .iter()
        .flat_map(|module| {
            module
                .imports
                .iter()
                .map(move |import| format!("{}!{}", module.name, import.symbol))
        })
        .collect();
    // The title imports more as the console API tests grow; these are the loader's own.
    for expected in [
        "kernel32.dll!GetCurrentThreadId",
        "standin_console.dll!StandInAdd",
        "standin_console.dll!StandInMode",
        "standin_console.dll!StandInReport",
        "standin_console.dll!StandInUnbuilt",
        "standin_ordinal.dll!#7",
    ] {
        assert!(
            imports.iter().any(|import| import == expected),
            "{expected} is missing from {imports:?}"
        );
    }

    let text = image.sections.iter().find(|s| s.name == ".text").unwrap();
    assert!(text.executable && !text.writable);
    let data = image.sections.iter().find(|s| s.name == ".data").unwrap();
    assert!(data.writable && !data.executable);

    assert!(image.relocatable);
    assert!(
        !image.relocations.is_empty(),
        "the image stores a pointer, so it has relocations"
    );
    let tls = image.tls.expect("the image has a TLS directory");
    assert!(tls.callbacks_rva.is_some());
    assert_eq!((tls.template_size, tls.zero_fill), (0, 0));
    assert!(image.exception_table.is_some());
    assert!(!image.has_delay_imports);
}

#[test]
fn the_stand_in_title_loads_binds_and_runs_to_its_exit() {
    let loaded = load(NAME, stand_in(), &resolver).unwrap();
    assert_eq!(loaded.name(), NAME);
    assert_eq!(loaded.tls_callback_count(), 1);

    take_reports();
    // SAFETY: the image is our own stand-in title and the resolver gave it the functions it
    // declares.
    let exit_code = unsafe { loaded.run() };

    assert_eq!(exit_code, EXIT_CODE);
    assert_eq!(take_reports(), expected_reports());
}

#[test]
fn two_copies_load_at_different_bases_and_both_run() {
    // Both cannot have the preferred base, so at least one is relocated. Its stored pointer
    // only reads the right value if the relocation was applied.
    let first = load(NAME, stand_in(), &resolver).unwrap();
    let second = load(NAME, stand_in(), &resolver).unwrap();
    assert_ne!(first.base(), second.base());
    let preferred = Image::parse(stand_in()).unwrap().preferred_base as usize;
    assert!(first.base() != preferred || second.base() != preferred);

    // The value the title reports through its pointer is the same in every copy, so an
    // unrelocated pointer into another copy would read it too. Read the pointers themselves:
    // every relocated slot must point into its own copy.
    let relocations = Image::parse(stand_in()).unwrap().relocations;
    for loaded in [&first, &second] {
        for &rva in &relocations {
            // SAFETY: the parser checked the slot lies in the image, which is mapped and
            // readable for as long as `loaded` lives.
            let pointer =
                unsafe { ((loaded.base() + rva as usize) as *const u64).read_unaligned() };
            assert!(
                loaded.contains(pointer as usize),
                "the slot at {rva:#x} holds {pointer:#x}, outside the copy at {:#x}",
                loaded.base()
            );
        }
    }

    for loaded in [&first, &second] {
        take_reports();
        // SAFETY: as in the test above.
        let exit_code = unsafe { loaded.run() };
        assert_eq!(exit_code, EXIT_CODE);
        assert_eq!(take_reports(), expected_reports());
    }
}

#[test]
fn writable_data_keeps_its_value_between_runs_of_one_image() {
    let loaded = load(NAME, stand_in(), &resolver).unwrap();
    take_reports();
    // SAFETY: as in the tests above.
    unsafe { loaded.run() };
    // SAFETY: as in the tests above.
    unsafe { loaded.run() };
    let counters: Vec<u64> = take_reports()
        .into_iter()
        .filter(|&(id, _)| id == REPORT_COUNTER)
        .map(|(_, value)| value)
        .collect();
    assert_eq!(counters, [101, 102]);
}

#[test]
fn every_missing_import_is_named() {
    let partial = |module: &str, symbol: &Symbol| match symbol {
        Symbol::Name(name) if name == "StandInAdd" => None,
        Symbol::Ordinal(_) => None,
        _ => resolver(module, symbol),
    };
    match load(NAME, stand_in(), &partial) {
        Err(LoadError::Unresolved(mut names)) => {
            names.sort();
            assert_eq!(
                names,
                ["standin_console.dll!StandInAdd", "standin_ordinal.dll!#7"]
            );
        }
        other => panic!("expected unresolved imports, got {other:?}"),
    }
}

#[test]
fn an_address_in_the_image_is_named_while_it_is_loaded() {
    let loaded = load("named-title.exe", stand_in(), &resolver).unwrap();
    let entry = loaded.entry_point();
    let offset = entry - loaded.base();
    assert!(loaded.contains(entry));
    assert_eq!(
        describe(entry),
        Some(format!("named-title.exe+{offset:#x}"))
    );
    assert!(!loaded.contains(loaded.base() + loaded.size()));

    drop(loaded);
    assert_eq!(describe(entry), None);
}

#[test]
fn a_truncated_image_is_refused_at_every_length() {
    let bytes = stand_in();
    for length in 0..bytes.len() {
        assert!(
            load(NAME, &bytes[..length], &resolver).is_err(),
            "length {length}"
        );
    }
}

#[test]
fn no_single_corrupt_byte_makes_the_parser_panic() {
    let bytes = stand_in();
    for index in 0..bytes.len() {
        let mut corrupt = bytes.to_vec();
        corrupt[index] ^= 0xFF;
        let _ = Image::parse(&corrupt);
    }
}

/// The body of the child process that `an_unhandled_fault_is_named_as_module_plus_offset`
/// starts. On its own it does nothing.
#[test]
fn fault_child() {
    if std::env::var_os(FAULT_CHILD).is_none() {
        return;
    }
    report_unhandled_faults();
    let loaded = load(NAME, stand_in(), &resolver).unwrap();
    println!("entry-offset {:#x}", loaded.entry_point() - loaded.base());
    MODE.set(MODE_FAULT);
    // SAFETY: as in the tests above. This run writes to address zero on purpose.
    unsafe { loaded.run() };
    unreachable!("the stand-in title faulted and the process ended");
}

#[test]
fn an_unhandled_fault_is_named_as_module_plus_offset() {
    // Build before starting the child, so the child finds the image ready.
    stand_in();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fault_child", "--nocapture", "--test-threads=1"])
        .env(FAULT_CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(!output.status.success(), "the child should have died");
    // Windows reports the fault's code as the exit code.
    assert_eq!(output.status.code(), Some(0xC000_0005u32 as i32));
    let line = stderr
        .lines()
        .find(|line| line.starts_with("riptide: unhandled fault"))
        .unwrap_or_else(|| panic!("no fault line in:\n{stderr}"));
    assert!(
        line.starts_with(
            "riptide: unhandled fault 0xc0000005 (access violation writing 0x0) at \
             stand-in-title.exe+0x"
        ),
        "{line}"
    );

    // The fault lies in the entry point's function: after its start, and not far.
    let offset = |text: &str| usize::from_str_radix(text.trim_start_matches("0x"), 16).unwrap();
    let entry = stdout
        .lines()
        // The test harness prints the test's name on the same line first.
        .find_map(|line| line.split("entry-offset ").nth(1))
        .map(offset)
        .expect("the child printed its entry offset");
    let fault = offset(line.rsplit('+').next().unwrap());
    assert!(
        fault > entry && fault < entry + 0x200,
        "fault at {fault:#x}, entry at {entry:#x}"
    );
}
