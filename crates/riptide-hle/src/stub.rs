//! Stubs for functions Riptide does not implement.
//!
//! A title has to load even when it imports functions that are not built yet: most are never
//! called. So each such import gets a small piece of code of its own. Calling it says which
//! function it stands for and stops the title. It never returns a value, because any value
//! would be a guess.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::sys;

/// The exit code a called stub ends the process with. Bit 29 marks it as not one of
/// Windows' own; the low bytes spell "RIP".
pub const UNIMPLEMENTED_IMPORT: u32 = 0xE052_4950;

/// Receives one line for each stub the first time it is called.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

const PAGE: usize = 0x1000;
/// Bytes of code per stub, and bytes of data per stub.
const SLOT: usize = 16;
const SLOTS_PER_BLOCK: usize = PAGE / SLOT;

/// The code of every stub. It reads its own entry from the data page that follows the code
/// page, then jumps to the handler stored beside it:
///
/// ```text
/// mov rcx, [rip + 0xFF9]   ; this stub's entry, the handler's argument
/// jmp [rip + 0xFFB]        ; the handler
/// int3 (x3)
/// ```
///
/// Both displacements reach one page ahead, so every stub has the same bytes and the code
/// page is never written again once it is executable.
const STUB_CODE: [u8; SLOT] = [
    0x48, 0x8B, 0x0D, 0xF9, 0x0F, 0x00, 0x00, // mov rcx, [rip + PAGE - 7]
    0xFF, 0x25, 0xFB, 0x0F, 0x00, 0x00, // jmp [rip + PAGE + 8 - 13]
    0xCC, 0xCC, 0xCC,
];

/// What one stub knows about itself. Its address is the stub's argument to the handler.
pub(crate) struct Entry {
    /// The import as `module!function`.
    name: String,
    called: AtomicBool,
    log: Log,
}

impl Entry {
    /// Logs the import's name, the first time only. Returns whether it logged.
    pub(crate) fn note_call(&self) -> bool {
        if self.called.swap(true, Ordering::Relaxed) {
            return false;
        }
        (self.log)(&format!("unimplemented import {} was called", self.name));
        true
    }
}

/// One page of stub code followed by one page of their data.
struct Block {
    base: *mut u8,
    used: usize,
}

impl Block {
    fn new() -> Option<Block> {
        // SAFETY: asks Windows for two new pages; touches no existing memory.
        let base = unsafe {
            sys::VirtualAlloc(
                std::ptr::null_mut(),
                2 * PAGE,
                sys::MEM_RESERVE | sys::MEM_COMMIT,
                sys::PAGE_READWRITE,
            )
        }
        .cast::<u8>();
        if base.is_null() {
            return None;
        }
        for slot in 0..SLOTS_PER_BLOCK {
            // SAFETY: the code page is `PAGE` writable bytes at `base`, and each slot lies
            // inside it.
            unsafe {
                std::ptr::copy_nonoverlapping(STUB_CODE.as_ptr(), base.add(slot * SLOT), SLOT)
            };
        }
        let mut old = 0u32;
        // SAFETY: the code page belongs to this block. From here on it is never written.
        let protected =
            unsafe { sys::VirtualProtect(base.cast(), PAGE, sys::PAGE_EXECUTE_READ, &mut old) };
        if protected == 0 {
            // SAFETY: `base` came from VirtualAlloc above and nothing else knows it yet.
            unsafe { sys::VirtualFree(base.cast(), 0, sys::MEM_RELEASE) };
            return None;
        }
        // SAFETY: the range is this block's code page.
        unsafe { sys::FlushInstructionCache(sys::GetCurrentProcess(), base.cast(), PAGE) };
        Some(Block { base, used: 0 })
    }

    /// Gives the next free stub its entry and returns the stub's address.
    fn add(&mut self, entry: *const Entry) -> usize {
        debug_assert!(self.used < SLOTS_PER_BLOCK);
        let slot = self.used;
        self.used += 1;
        // SAFETY: the data page is the second page of the block and stays writable; the slot
        // lies inside it and is aligned to 16 bytes.
        unsafe {
            let data = self.base.add(PAGE + slot * SLOT).cast::<usize>();
            data.write(entry as usize);
            data.add(1).write(unimplemented as *const () as usize);
            self.base.add(slot * SLOT) as usize
        }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: `base` came from VirtualAlloc and is released once, here.
        unsafe { sys::VirtualFree(self.base.cast(), 0, sys::MEM_RELEASE) };
    }
}

/// Every stub made so far. Dropping it frees their code, so it has to outlive every image
/// bound to them.
pub(crate) struct Stubs {
    log: Log,
    blocks: Vec<Block>,
    /// Boxed so each entry keeps its address while the list grows: stubs point at them.
    #[allow(clippy::vec_box)]
    entries: Vec<Box<Entry>>,
}

// SAFETY: a block's pointer is only used through `&mut self`, and the memory behind it
// belongs to this table alone.
unsafe impl Send for Stubs {}

impl Stubs {
    pub(crate) fn new(log: Log) -> Stubs {
        Stubs {
            log,
            blocks: Vec::new(),
            entries: Vec::new(),
        }
    }

    /// Makes a stub for the import called `name` (`module!function`) and returns its
    /// address, or `None` if Windows gives no memory for it.
    pub(crate) fn add(&mut self, name: String) -> Option<usize> {
        if self.blocks.last().is_none_or(|b| b.used == SLOTS_PER_BLOCK) {
            self.blocks.push(Block::new()?);
        }
        let entry = Box::new(Entry {
            name,
            called: AtomicBool::new(false),
            log: Arc::clone(&self.log),
        });
        let address = self
            .blocks
            .last_mut()
            .expect("a block was just made")
            .add(&raw const *entry);
        self.entries.push(entry);
        Some(address)
    }
}

/// Where every stub jumps: names the import, once, and stops the title.
///
/// It ends the process with [`UNIMPLEMENTED_IMPORT`] as its exit code. It does not raise an
/// exception: a title's own handler (a crash reporter around its main loop) could take one
/// and carry on as if the call had returned.
unsafe extern "system" fn unimplemented(entry: *const Entry) -> ! {
    // SAFETY: a stub passes the entry stored for it, which lives as long as the stub does.
    let entry = unsafe { &*entry };
    // One thread at a time, and the lock is never given back: a second thread that reaches
    // a stub while the first is still writing its lines must not end the process under it.
    static STOPPING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _stopping = STOPPING.lock().unwrap_or_else(|poison| poison.into_inner());
    // A log that panics must not change how the process ends.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if entry.note_call() {
            // A stub jumps here, so the first return address on the stack that lies in a
            // loaded image is the title's: the instruction after its call. The frames
            // before it are this function's own.
            let mut frames = [std::ptr::null_mut::<std::ffi::c_void>(); 16];
            // SAFETY: `frames` has room for the frames asked for.
            let found = unsafe {
                sys::RtlCaptureStackBackTrace(
                    0,
                    frames.len() as u32,
                    frames.as_mut_ptr(),
                    std::ptr::null_mut(),
                )
            };
            let place = frames[..usize::from(found).min(frames.len())]
                .iter()
                .find_map(|&frame| riptide_pe::describe(frame as usize));
            // No title frame is on the stack when the title reached the import by a jump
            // (a tail call) from a function Riptide called.
            if let Some(place) = place {
                (entry.log)(&format!("the call returns to {place}"));
            }
        }
    }));
    // SAFETY: ending this process is always allowed.
    unsafe { sys::TerminateProcess(sys::GetCurrentProcess(), UNIMPLEMENTED_IMPORT) };
    std::process::abort()
}

/// Stops the title because it asked for something that is not built, naming it: for what
/// is found unbuilt in the middle of a call, where there is no stub to jump to.
pub(crate) fn stop(what: &str) -> ! {
    let line = format!("unimplemented: {what}");
    let _ = std::panic::catch_unwind(|| {
        eprintln!("riptide: {line}");
        crate::trace::trace!("{line}");
    });
    // SAFETY: ending this process is always allowed.
    unsafe { sys::TerminateProcess(sys::GetCurrentProcess(), UNIMPLEMENTED_IMPORT) };
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn recording() -> (Log, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: Log = Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned()));
        (log, lines)
    }

    #[test]
    fn a_stub_logs_its_name_the_first_time_only() {
        let (log, lines) = recording();
        let entry = Entry {
            name: "console.dll!NotBuilt".to_owned(),
            called: AtomicBool::new(false),
            log,
        };
        assert!(entry.note_call());
        assert!(!entry.note_call());
        assert!(!entry.note_call());
        assert_eq!(
            *lines.lock().unwrap(),
            ["unimplemented import console.dll!NotBuilt was called"]
        );
    }

    #[test]
    fn each_stub_has_its_own_address_and_entry() {
        let (log, _) = recording();
        let mut stubs = Stubs::new(log);
        let first = stubs.add("a.dll!One".to_owned()).unwrap();
        let second = stubs.add("a.dll!Two".to_owned()).unwrap();
        assert_eq!(second - first, SLOT);

        // The data slot one page past each stub holds that stub's entry and the handler.
        for (address, name) in [(first, "a.dll!One"), (second, "a.dll!Two")] {
            // SAFETY: the data page of a live block is readable, and its slots hold what
            // `Block::add` wrote.
            let (entry, handler) = unsafe {
                let data = (address + PAGE) as *const usize;
                (&*(data.read() as *const Entry), data.add(1).read())
            };
            assert_eq!(entry.name, name);
            assert_eq!(handler, unimplemented as *const () as usize);
        }
    }

    #[test]
    fn more_stubs_than_one_block_holds_get_a_second_block() {
        let (log, _) = recording();
        let mut stubs = Stubs::new(log);
        let mut addresses = std::collections::HashSet::new();
        for index in 0..SLOTS_PER_BLOCK + 3 {
            assert!(addresses.insert(stubs.add(format!("a.dll!F{index}")).unwrap()));
        }
        assert_eq!(stubs.blocks.len(), 2);
    }

    #[test]
    fn the_stub_code_reaches_one_page_ahead() {
        // mov rcx, [rip + d]: the instruction ends at byte 7 and must read slot + PAGE.
        let mov = u32::from_le_bytes(STUB_CODE[3..7].try_into().unwrap()) as usize;
        assert_eq!(7 + mov, PAGE);
        // jmp [rip + d]: the instruction ends at byte 13 and must read slot + PAGE + 8.
        let jmp = u32::from_le_bytes(STUB_CODE[9..13].try_into().unwrap()) as usize;
        assert_eq!(13 + jmp, PAGE + 8);
    }
}
