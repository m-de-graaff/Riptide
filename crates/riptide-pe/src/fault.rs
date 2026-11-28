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
    let in_title = match MODULES.try_lock() {
        Ok(modules) => describe_in(&modules, address),
        Err(_) => None,
    };
    if record.ExceptionCode == sys::EXCEPTION_BREAKPOINT
        && let Some(place) = &in_title
    {
        // SAFETY: Windows passes the faulting thread's context with the exception, and
        // the address is the title's code, which is readable.
        if unsafe { step_over_breakpoint(address, (*pointers).ContextRecord.cast::<u8>(), place) } {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }
    let place = in_title.or_else(|| host_place(address));
    let line = fault_line(record.ExceptionCode, address, access, place);
    // SAFETY: Windows passes the faulting thread's context with the exception.
    let more = unsafe { registers_and_callers((*pointers).ContextRecord.cast::<u8>()) };
    let mut stderr = std::io::stderr().lock();
    for line in std::iter::once(&line).chain(&more) {
        let _ = writeln!(stderr, "{line}");
        if let Some(sink) = SINK.get() {
            sink(line.trim_start_matches("riptide: "));
        }
    }
    let _ = stderr.flush();
    sys::EXCEPTION_EXECUTE_HANDLER
}

/// What a filter answers to have the thread go on from its context.
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;

/// How many of a title's breakpoints are stepped over before one stops it: a title that
/// breaks without end is not running.
const BREAKPOINTS: u32 = 4096;

/// Steps over a breakpoint in a title's own code, if that is what is at `address`: the
/// thread's context is moved past the one-byte instruction, and the place is said in the
/// session log the first few times.
///
/// A title's checks of its own state end in a breakpoint with the code to go on after it.
/// On the console a debugger, or nothing at all, takes the thread past it; here nothing
/// handles it and the title would end. So it is stepped over, as a debugger told to go on
/// would.
///
/// # Safety
/// `address` is readable, and `context` is null or the x64 `CONTEXT` of the stopped thread.
unsafe fn step_over_breakpoint(address: usize, context: *mut u8, place: &str) -> bool {
    use std::sync::atomic::{AtomicU32, Ordering};
    static STEPPED: AtomicU32 = AtomicU32::new(0);
    /// The one-byte breakpoint instruction.
    const INT3: u8 = 0xCC;
    /// Where the instruction pointer is in the context.
    const RIP: usize = 0xF8;
    // SAFETY: readable, as the caller says.
    if context.is_null() || unsafe { (address as *const u8).read() } != INT3 {
        return false;
    }
    let stepped = STEPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if stepped > BREAKPOINTS {
        return false;
    }
    if stepped <= 16 || stepped.is_power_of_two() {
        let line =
            format!("the title's own breakpoint at {place} is stepped over ({stepped} so far)");
        eprintln!("riptide: {line}");
        if let Some(sink) = SINK.get() {
            sink(&line);
        }
    }
    // SAFETY: the context is the stopped thread's; the instruction pointer goes past the
    // one byte of the breakpoint.
    unsafe {
        context
            .add(RIP)
            .cast::<u64>()
            .write_unaligned(address as u64 + 1);
    }
    true
}

/// More lines for a fault: the registers a call passes its first arguments in, the other
/// general registers, and the addresses on the faulting thread's stack that lie in a loaded image. Those are mostly
/// return addresses, nearest caller first, though a stale one can be among them.
///
/// # Safety
/// `context` is the x64 `CONTEXT` of a thread that is stopped.
unsafe fn registers_and_callers(context: *const u8) -> Vec<String> {
    if context.is_null() {
        return Vec::new();
    }
    // Offsets of the general registers in the x64 CONTEXT.
    // SAFETY: the context is 1232 bytes; every offset read lies inside it.
    let register = |offset: usize| unsafe { context.add(offset).cast::<u64>().read_unaligned() };
    let (rax, rcx, rdx, rsp) = (
        register(0x78),
        register(0x80),
        register(0x88),
        register(0x98),
    );
    let (r8, r9) = (register(0xB8), register(0xC0));
    let mut lines = vec![format!(
        "riptide: rax {rax:#x} rcx {rcx:#x} rdx {rdx:#x} r8 {r8:#x} r9 {r9:#x} rsp {rsp:#x}"
    )];
    // The registers a function keeps across calls: what it was working on when it faulted.
    lines.push(format!(
        "riptide: rbx {:#x} rbp {:#x} rsi {:#x} rdi {:#x} r10 {:#x} r11 {:#x} r12 {:#x} r13 {:#x}          r14 {:#x} r15 {:#x}",
        register(0x90),
        register(0xA0),
        register(0xA8),
        register(0xB0),
        register(0xC8),
        register(0xD0),
        register(0xD8),
        register(0xE0),
        register(0xE8),
        register(0xF0),
    ));
    // A register that points into one of Windows' modules is named: it is often what tells
    // an argument of the wrong kind from a wrong value.
    for (name, value) in [
        ("rax", rax),
        ("rcx", rcx),
        ("rdx", rdx),
        ("r8", r8),
        ("r9", r9),
    ] {
        if let Some(place) = host_place(value as usize) {
            lines.push(format!("riptide: {name} points at {place}"));
        }
    }

    let mut callers = Vec::new();
    if let Ok(modules) = MODULES.try_lock() {
        for slot in 0..512usize {
            let at = (rsp as usize).wrapping_add(slot * 8) as *const usize;
            let mut info = [0usize; 6];
            // SAFETY: any address may be queried; the buffer is the size passed.
            let known = unsafe { sys::VirtualQuery(at.cast(), info.as_mut_ptr().cast(), 48) };
            // State (committed) is the low half of the fifth word, protection the high half
            // of it: stop where the stack is no longer readable.
            const MEM_COMMIT: usize = 0x1000;
            let state = info[4] & 0xFFFF_FFFF;
            let protection = (info[4] >> 32) & 0xFF;
            if known == 0 || state != MEM_COMMIT || protection & 0x66 == 0 {
                break;
            }
            // SAFETY: the page was just found committed and readable.
            let value = unsafe { at.read_unaligned() };
            if let Some(place) = describe_in(&modules, value) {
                callers.push(place);
                if callers.len() == 10 {
                    break;
                }
            }
        }
    }
    if !callers.is_empty() {
        lines.push(format!("riptide: on the stack: {}", callers.join(", ")));
    }
    lines
}

/// Names `address` as `module+0xoffset` if it lies in a module Windows loaded.
fn host_place(address: usize) -> Option<String> {
    const FROM_ADDRESS: u32 = 4;
    const UNCHANGED_REFCOUNT: u32 = 2;
    let mut module = std::ptr::null_mut();
    // SAFETY: with FROM_ADDRESS the second argument is an address to look up, not a string.
    let found = unsafe {
        sys::GetModuleHandleExW(
            FROM_ADDRESS | UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    };
    if found == 0 || module.is_null() {
        return None;
    }
    let mut path = [0u16; 512];
    // SAFETY: the buffer is as long as the size passed.
    let length = unsafe { sys::GetModuleFileNameW(module, path.as_mut_ptr(), 512) } as usize;
    let path = String::from_utf16_lossy(&path[..length.min(512)]);
    let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
    Some(format!("{name}+{:#x}", address - module as usize))
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
