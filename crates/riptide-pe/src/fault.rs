//! Naming a fault as module plus offset.
//!
//! A raw address means nothing from one run to the next: the image moves. `title.exe+0x1a2b`
//! is the same place every time, and is what a crash report has to say.

use std::io::Write;
use std::sync::{Mutex, Once, OnceLock};

use crate::sys;

struct Module {
    name: String,
    base: usize,
    size: usize,
}

/// Every image loaded right now.
static MODULES: Mutex<Vec<Module>> = Mutex::new(Vec::new());

pub(crate) fn register(name: &str, base: usize, size: usize) {
    let mut modules = MODULES.lock().unwrap_or_else(|poison| poison.into_inner());
    modules.push(Module {
        name: name.to_owned(),
        base,
        size,
    });
}

pub(crate) fn unregister(base: usize) {
    let mut modules = MODULES.lock().unwrap_or_else(|poison| poison.into_inner());
    modules.retain(|module| module.base != base);
}

/// Names `address` as `module+0xoffset` if it lies in a loaded image.
pub fn describe(address: usize) -> Option<String> {
    let modules = MODULES.lock().unwrap_or_else(|poison| poison.into_inner());
    describe_in(&modules, address)
}

fn describe_in(modules: &[Module], address: usize) -> Option<String> {
    modules
        .iter()
        .find(|module| address >= module.base && address - module.base < module.size)
        .map(|module| format!("{}+{:#x}", module.name, address - module.base))
}

/// What an exception code means, for the ones a title's fault usually is.
fn code_name(code: u32) -> &'static str {
    match code {
        sys::EXCEPTION_ACCESS_VIOLATION => "access violation",
        sys::EXCEPTION_IN_PAGE_ERROR => "page could not be read from disk",
        sys::EXCEPTION_ILLEGAL_INSTRUCTION => "illegal instruction",
        sys::EXCEPTION_PRIV_INSTRUCTION => "privileged instruction",
        sys::EXCEPTION_INT_DIVIDE_BY_ZERO => "integer divide by zero",
        sys::EXCEPTION_STACK_OVERFLOW => "stack overflow",
        sys::EXCEPTION_BREAKPOINT => "breakpoint",
        _ => "exception",
    }
}

/// One line that says what went wrong and where.
///
/// `access` is the first two parameters of an access violation: what was tried (0 read,
/// 1 write, 8 execute) and at which address.
fn fault_line(
    code: u32,
    address: usize,
    access: Option<(usize, usize)>,
    place: Option<String>,
) -> String {
    let mut line = format!("riptide: unhandled fault {code:#010x} ({}", code_name(code));
    if let Some((kind, target)) = access {
        let verb = match kind {
            0 => "reading",
            1 => "writing",
            8 => "executing",
            _ => "touching",
        };
        line.push_str(&format!(" {verb} {target:#x}"));
    }
    match place {
        Some(place) => line.push_str(&format!(") at {place}")),
        None => line.push_str(&format!(") at {address:#x}, outside every loaded image")),
    }
    line
}

/// Runs when an exception has found no handler anywhere, just before the process ends.
unsafe extern "system" fn unhandled(pointers: *mut sys::EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows passes valid exception pointers to an unhandled-exception filter.
    let record = unsafe { &*(*pointers).ExceptionRecord };
    let address = record.ExceptionAddress as usize;
    let access = matches!(
        record.ExceptionCode,
        sys::EXCEPTION_ACCESS_VIOLATION | sys::EXCEPTION_IN_PAGE_ERROR
    )
    .then(|| {
        (
            record.ExceptionInformation[0],
            record.ExceptionInformation[1],
        )
    })
    .filter(|_| record.NumberParameters >= 2);

    // The fault may have come while this thread held the lock; do not wait for it then.
    let place = match MODULES.try_lock() {
        Ok(modules) => describe_in(&modules, address),
        Err(_) => None,
    };
    let line = fault_line(record.ExceptionCode, address, access, place);
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
    let _ = stderr.flush();
    if let Some(sink) = SINK.get() {
        sink(line.trim_start_matches("riptide: "));
    }
    sys::EXCEPTION_EXECUTE_HANDLER
}

/// Also receives the line of an unhandled fault, without the `riptide: ` prefix.
static SINK: OnceLock<Sink> = OnceLock::new();

type Sink = Box<dyn Fn(&str) + Send + Sync>;

/// As [`report_unhandled_faults`], and hands the line to `sink` as well, so it can go into
/// the session log. `sink` runs in a process that has just faulted: it should do little.
/// Only the first sink given is kept.
pub fn report_unhandled_faults_to(sink: impl Fn(&str) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(sink));
    report_unhandled_faults();
}

/// Makes a fault that nothing handles print one line to standard error, naming where it
/// happened as module plus offset, before the process ends with the fault's code.
///
/// Only unhandled faults are reported. A fault the title's own handlers or Riptide's write
/// guards deal with never reaches this. Calling it more than once does nothing more.
pub fn report_unhandled_faults() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        // SAFETY: `unhandled` has the signature Windows calls and lives as long as the
        // process.
        unsafe { sys::SetUnhandledExceptionFilter(Some(unhandled)) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modules() -> Vec<Module> {
        vec![
            Module {
                name: "title.exe".to_owned(),
                base: 0x1_4000_0000,
                size: 0x6000,
            },
            Module {
                name: "other.dll".to_owned(),
                base: 0x1_8000_0000,
                size: 0x1000,
            },
        ]
    }

    #[test]
    fn an_address_in_an_image_is_named_as_module_plus_offset() {
        let modules = modules();
        assert_eq!(
            describe_in(&modules, 0x1_4000_1a2b).as_deref(),
            Some("title.exe+0x1a2b")
        );
        assert_eq!(
            describe_in(&modules, 0x1_8000_0000).as_deref(),
            Some("other.dll+0x0")
        );
    }

    #[test]
    fn an_address_outside_every_image_has_no_name() {
        let modules = modules();
        assert_eq!(describe_in(&modules, 0x1_4000_6000), None);
        assert_eq!(describe_in(&modules, 0x1000), None);
    }

    #[test]
    fn a_fault_line_says_what_where_and_which_access() {
        assert_eq!(
            fault_line(
                sys::EXCEPTION_ACCESS_VIOLATION,
                0x1_4000_10a4,
                Some((1, 0)),
                Some("title.exe+0x10a4".to_owned())
            ),
            "riptide: unhandled fault 0xc0000005 (access violation writing 0x0) at \
             title.exe+0x10a4"
        );
        assert_eq!(
            fault_line(sys::EXCEPTION_ILLEGAL_INSTRUCTION, 0x7ff0_1234, None, None),
            "riptide: unhandled fault 0xc000001d (illegal instruction) at 0x7ff01234, outside \
             every loaded image"
        );
    }
}
