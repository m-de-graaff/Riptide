//! Binds the stand-in title through the import table and runs it.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use riptide_hle::{Binding, Imports, UNIMPLEMENTED_IMPORT};
use riptide_pe::{Image, Symbol, load, report_unhandled_faults};

const NAME: &str = "stand-in-title.exe";
const EXIT_CODE: u32 = 42;

// Report ids and modes. The stand-in title's source holds the same numbers.
const REPORT_THREAD: u32 = 7;
const MODE_UNBUILT: u32 = 2;
const MODE_SLEEP: u32 = 3;
const REPORT_SLEPT: u32 = 8;

/// Set in the child process that the stub test starts.
const STUB_CHILD: &str = "RIPTIDE_HLE_STUB_CHILD";

/// Builds the stand-in title once and returns its bytes. `riptide-pe`'s tests hold the same
/// helper; it moves to a shared place when a third crate needs it.
fn stand_in() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/stand-in-title");
        let target = fixture.join("target");
        let output = Command::new(env!("CARGO"))
            .arg("build")
            .arg("--manifest-path")
            .arg(fixture.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .expect("cargo runs");
        assert!(
            output.status.success(),
            "the stand-in title did not build:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::read(target.join("debug").join(NAME)).expect("the built image is readable")
    })
}

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

#[link(name = "kernel32")]
unsafe extern "system" {
    safe fn GetCurrentThreadId() -> u32;
}

fn name(name: &str) -> Symbol {
    Symbol::Name(name.to_owned())
}

/// The table for the stand-in title: four functions implemented, one passed to the host,
/// and `StandInUnbuilt` left out, as a console function nobody has built would be.
fn imports() -> Imports {
    let mut imports = Imports::new();
    // SAFETY: each function has the signature the stand-in title declares for that import.
    unsafe {
        imports.implement(
            "standin_console.dll",
            name("StandInReport"),
            stand_in_report as *const () as usize,
        );
        imports.implement(
            "standin_console.dll",
            name("StandInAdd"),
            stand_in_add as *const () as usize,
        );
        imports.implement(
            "standin_console.dll",
            name("StandInMode"),
            stand_in_mode as *const () as usize,
        );
        imports.implement(
            "standin_ordinal.dll",
            Symbol::Ordinal(7),
            stand_in_by_ordinal as *const () as usize,
        );
    }
    imports.pass_through("kernel32.dll", "GetCurrentThreadId");
    imports
}

#[test]
fn the_census_lists_every_import_by_what_it_gets_without_running_the_title() {
    let image = Image::parse(stand_in()).unwrap();
    let census = imports().census(&image);

    assert_eq!(
        census.to_string(),
        "host    kernel32.dll!GetCurrentThreadId\n\
         riptide kernel32.dll!Sleep\n\
         riptide standin_console.dll!StandInAdd\n\
         riptide standin_console.dll!StandInMode\n\
         riptide standin_console.dll!StandInReport\n\
         stub    standin_console.dll!StandInUnbuilt\n\
         riptide standin_ordinal.dll!#7\n\
         7 imports: 5 implemented by Riptide, 1 passed to the host, 1 stubs"
    );
    assert_eq!(census.count(Binding::Riptide), 5);
    assert_eq!(census.count(Binding::Host), 1);
    assert_eq!(census.count(Binding::Stub), 1);
    // Nothing ran: the title reports as soon as it runs.
    assert!(REPORTS.take().is_empty());
}

#[test]
fn with_an_empty_table_every_import_is_a_stub() {
    let image = Image::parse(stand_in()).unwrap();
    let census = Imports::empty(std::sync::Arc::new(|_: &str| {})).census(&image);
    assert_eq!(census.count(Binding::Stub), census.lines.len());
    assert_eq!(census.lines.len(), 7);
}

#[test]
fn a_1_ms_sleep_from_the_title_takes_under_2_ms_over_1000_calls() {
    let imports = imports();
    let loaded = load(NAME, stand_in(), &imports).unwrap();

    REPORTS.take();
    MODE.set(MODE_SLEEP);
    let start = std::time::Instant::now();
    // SAFETY: as in the test above. In this mode the title calls Sleep(1) a thousand times.
    let exit_code = unsafe { loaded.run() };
    let took = start.elapsed();
    MODE.set(0);

    assert_eq!(exit_code, EXIT_CODE);
    let slept = REPORTS
        .take()
        .into_iter()
        .find(|&(id, _)| id == REPORT_SLEPT)
        .map(|(_, value)| value);
    assert_eq!(slept, Some(1000));

    let mean = took / 1000;
    println!("1 ms sleep from the title over 1000 calls: mean {mean:?}");
    assert!(
        mean >= std::time::Duration::from_micros(900),
        "mean {mean:?}"
    );
    // A shared CI runner's timers jitter too much for the upper bound.
    if std::env::var_os("CI").is_none() {
        assert!(mean < std::time::Duration::from_millis(2), "mean {mean:?}");
    }
}

#[test]
fn the_title_loads_with_an_unbuilt_import_and_runs_when_it_does_not_call_it() {
    let imports = imports();
    let loaded = load(NAME, stand_in(), &imports).unwrap();

    REPORTS.take();
    // SAFETY: the image is our own stand-in title, bound to functions of the signatures it
    // declares; in this mode it does not call its unbuilt import.
    let exit_code = unsafe { loaded.run() };

    assert_eq!(exit_code, EXIT_CODE);
    // The passed-through import is the host's own function: it names this very thread.
    let thread = REPORTS
        .take()
        .into_iter()
        .find(|&(id, _)| id == REPORT_THREAD)
        .map(|(_, value)| value);
    assert_eq!(thread, Some(u64::from(GetCurrentThreadId())));
}

/// The body of the child process that `calling_a_stub_logs_its_name_once_and_fails` starts.
/// On its own it does nothing.
#[test]
fn stub_child() {
    if std::env::var_os(STUB_CHILD).is_none() {
        return;
    }
    report_unhandled_faults();
    let imports = imports();
    let loaded = load(NAME, stand_in(), &imports).unwrap();
    MODE.set(MODE_UNBUILT);
    // SAFETY: as above. This run calls the unbuilt import on purpose.
    unsafe { loaded.run() };
    unreachable!("the stub stopped the title and the process ended");
}

#[test]
fn calling_a_stub_logs_its_name_once_and_fails() {
    stand_in();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stub_child", "--nocapture", "--test-threads=1"])
        .env(STUB_CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(UNIMPLEMENTED_IMPORT as i32),
        "{stderr}"
    );
    let named: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("unimplemented import"))
        .collect();
    assert_eq!(
        named,
        ["riptide: unimplemented import standin_console.dll!StandInUnbuilt was called"],
        "{stderr}"
    );
}
