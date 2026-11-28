//! The title's file calls.
//!
//! A title names its files by the console's drives. `G:` is the title's own package, which
//! here is the game folder. The package is read-only on the console, and it is here: a call
//! that would write to it, or create or delete anything in it, is refused, so a title can
//! never change the game's files.
//!
//! `T:` is the title's scratch drive, which the console gives it to write what it likes.
//! Here it is the folder `%LOCALAPPDATA%\Riptide\titles\<package identity>\scratch`.
//!
//! Each call that names a file is written to the session log with its result. Other drives
//! are passed to Windows as they are until it is known what a title keeps on them.

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::sys;
use crate::trace::trace;

const ERROR_ACCESS_DENIED: u32 = 5;
const OPEN_EXISTING: u32 = 3;
/// `GENERIC_WRITE`, `GENERIC_ALL`, `DELETE`, and the rights to write or append data.
const WRITING: u32 = 0x4000_0000 | 0x1000_0000 | 0x0001_0000 | 0x2 | 0x4;
/// What Windows returns for a handle it could not make.
const INVALID_HANDLE: *mut c_void = usize::MAX as *mut c_void;

/// A null-terminated UTF-16 string as text.
///
/// # Safety
/// `text` is null or null-terminated.
pub(crate) unsafe fn wide(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut length = 0;
    // SAFETY: the caller vouches for the terminator.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, length))
    }
}

/// A null-terminated string as text.
///
/// # Safety
/// `text` is null or null-terminated.
unsafe fn narrow(text: *const c_char) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the caller vouches for the terminator.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

/// What follows the drive in `path`, if the path is on drive `letter`.
fn on_drive(path: &str, letter: u8) -> Option<&str> {
    let bytes = path.as_bytes();
    let on = bytes.len() >= 2
        && bytes[0].eq_ignore_ascii_case(&letter)
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'\\' || bytes[2] == b'/');
    on.then(|| path[2..].trim_start_matches(['\\', '/']))
}

/// Where a path the title gave leads: the text to pass to Windows, and whether it lies in
/// the title's package. `package` is the game folder and `scratch` the folder that stands
/// for the scratch drive; a drive whose folder is not known is left as it is.
fn lead(path: &str, package: Option<&Path>, scratch: Option<&Path>) -> (String, bool) {
    if let (Some(rest), Some(package)) = (on_drive(path, b'g'), package) {
        return (package.join(rest).to_string_lossy().into_owned(), true);
    }
    if let (Some(rest), Some(scratch)) = (on_drive(path, b't'), scratch) {
        return (scratch.join(rest).to_string_lossy().into_owned(), false);
    }
    (path.to_owned(), false)
}

/// The folder that stands for the title's scratch drive, made the first time it is asked
/// for. `None` if it cannot be made.
fn scratch_folder() -> Option<&'static Path> {
    static FOLDER: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let base = std::env::var_os("LOCALAPPDATA")?;
            let identity = crate::package::config()
                .and_then(|config| crate::package::attribute(config, "Identity", "Name"));
            let folder =
                crate::localstorage::title_data(Path::new(&base), identity.unwrap_or("unknown"))
                    .join("scratch");
            match std::fs::create_dir_all(&folder) {
                Ok(()) => {
                    trace!("the scratch drive T: is {}", folder.display());
                    Some(folder)
                }
                Err(error) => {
                    trace!(
                        "the scratch drive's folder {} cannot be made: {error}",
                        folder.display()
                    );
                    None
                }
            }
        })
        .as_deref()
}

/// [`lead`] for the title that is running, as a null-terminated UTF-16 string.
pub(crate) fn lead_wide(path: &str) -> (Vec<u16>, bool) {
    // The scratch folder is only made once a title names its drive.
    let scratch = on_drive(path, b't').and_then(|_| scratch_folder());
    let (led, in_package) = lead(path, crate::process::title_folder().as_deref(), scratch);
    (led.encode_utf16().chain([0]).collect(), in_package)
}

fn wide_z(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// Windows' reason for the last failure on this thread, kept across the logging.
fn failure() -> String {
    // SAFETY: the call has no preconditions.
    let code = unsafe { sys::GetLastError() };
    let text = format!("failed, error {code}");
    // SAFETY: as above. Logging may have changed the code the title is about to read.
    unsafe { sys::SetLastError(code) };
    text
}

/// The title's `CreateFileW`.
///
/// # Safety
/// The arguments are the title's own, as `CreateFileW` takes them.
unsafe extern "system" fn create_file_w(
    name: *const u16,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: *mut c_void,
) -> *mut c_void {
    // SAFETY: the title's string.
    let path = unsafe { wide(name) };
    let (led, in_package) = lead_wide(&path);
    if in_package && (access & WRITING != 0 || disposition != OPEN_EXISTING) {
        trace!(
            "CreateFileW({path}, access {access:#x}, disposition {disposition}) refused: the \
             package is read-only"
        );
        // SAFETY: the call has no preconditions.
        unsafe { sys::SetLastError(ERROR_ACCESS_DENIED) };
        return INVALID_HANDLE;
    }
    // SAFETY: the title's arguments, with the path led to where it lies on this machine.
    let handle = unsafe {
        sys::CreateFileW(
            led.as_ptr(),
            access,
            share,
            security,
            disposition,
            host_flags(flags),
            template,
        )
    };
    let result = if handle == INVALID_HANDLE {
        failure()
    } else {
        "opened".to_owned()
    };
    trace!(
        "CreateFileW({path}, access {access:#x}, disposition {disposition}, flags {flags:#x}) \
         {result}"
    );
    handle
}

/// `FILE_FLAG_NO_BUFFERING`: reads that go past Windows' file cache.
const NO_BUFFERING: u32 = 0x2000_0000;

/// The flags a title's file is opened with on this machine: the title's, without
/// [`NO_BUFFERING`].
///
/// A title opens its packages unbuffered and then reads them at whatever offsets its own
/// tables give. The console lets it; Windows refuses an unbuffered read that does not
/// start and end on a sector of the disc, and the title is left without what it read
/// for. Buffered, every read is served. What that costs is Windows' cache holding what
/// the title also holds, which Windows gives up when memory is short.
fn host_flags(flags: u32) -> u32 {
    flags & !NO_BUFFERING
}

/// The title's `GetFileAttributesW`.
///
/// # Safety
/// `name` is a null-terminated UTF-16 string.
unsafe extern "system" fn get_file_attributes_w(name: *const u16) -> u32 {
    // SAFETY: the title's string.
    let path = unsafe { wide(name) };
    let (led, _) = lead_wide(&path);
    // SAFETY: a null-terminated path.
    let attributes = unsafe { sys::GetFileAttributesW(led.as_ptr()) };
    let result = if attributes == u32::MAX {
        failure()
    } else {
        format!("{attributes:#x}")
    };
    trace!("GetFileAttributesW({path}) {result}");
    attributes
}

/// The title's `GetFileAttributesExW`.
///
/// # Safety
/// The arguments are the title's own, as `GetFileAttributesExW` takes them.
unsafe extern "system" fn get_file_attributes_ex_w(
    name: *const u16,
    level: u32,
    out: *mut c_void,
) -> i32 {
    // SAFETY: the title's string.
    let path = unsafe { wide(name) };
    let (led, _) = lead_wide(&path);
    // SAFETY: a null-terminated path and the title's own out-pointer.
    let found = unsafe { sys::GetFileAttributesExW(led.as_ptr(), level, out) };
    let result = if found == 0 {
        failure()
    } else {
        "found".to_owned()
    };
    trace!("GetFileAttributesExW({path}) {result}");
    found
}

/// The title's `FindFirstFileW`.
///
/// # Safety
/// The arguments are the title's own, as `FindFirstFileW` takes them.
unsafe extern "system" fn find_first_file_w(name: *const u16, out: *mut c_void) -> *mut c_void {
    // SAFETY: the title's string.
    let path = unsafe { wide(name) };
    let (led, _) = lead_wide(&path);
    // SAFETY: a null-terminated path and the title's own out-pointer.
    let handle = unsafe { sys::FindFirstFileW(led.as_ptr(), out) };
    let result = if handle == INVALID_HANDLE {
        failure()
    } else {
        "found".to_owned()
    };
    trace!("FindFirstFileW({path}) {result}");
    handle
}

/// Opens a file for the C runtime's `fopen` and `_wfopen`.
fn open(function: &str, path: &str, mode: &str) -> *mut c_void {
    let (led, in_package) = lead_wide(path);
    if in_package && mode.contains(['w', 'a', '+']) {
        trace!("{function}({path}, {mode}) refused: the package is read-only");
        return std::ptr::null_mut();
    }
    let mode_wide = wide_z(mode);
    // SAFETY: both strings are null-terminated.
    let file = unsafe { sys::_wfopen(led.as_ptr(), mode_wide.as_ptr()) };
    trace!(
        "{function}({path}, {mode}) {}",
        if file.is_null() { "failed" } else { "opened" }
    );
    file
}

/// The C runtime's `fopen`.
///
/// # Safety
/// Both arguments are null-terminated strings.
unsafe extern "C" fn fopen(name: *const c_char, mode: *const c_char) -> *mut c_void {
    // SAFETY: the title's strings.
    let (path, mode) = unsafe { (narrow(name), narrow(mode)) };
    open("fopen", &path, &mode)
}

/// The C runtime's `_wfopen`.
///
/// # Safety
/// Both arguments are null-terminated UTF-16 strings.
unsafe extern "C" fn wfopen(name: *const u16, mode: *const u16) -> *mut c_void {
    // SAFETY: the title's strings.
    let (path, mode) = unsafe { (wide(name), wide(mode)) };
    open("_wfopen", &path, &mode)
}

/// The title's `ReadFile`.
///
/// A read into title memory is a write there that page protection cannot hear: Windows
/// writes from inside a system call, which fails on a protected page instead of faulting.
/// The graphics runtime protects the pages of textures to learn when they are written, so
/// it is told before the read. A read that Windows would finish later (an overlapped one)
/// is waited for here before this returns, still saying that it is pending as Windows
/// did: otherwise a page protected in the meantime would make the read fail when it
/// lands. The title sees the read finish as it would have, only sooner.
///
/// # Safety
/// The arguments are the title's own, as `ReadFile` takes them.
unsafe extern "system" fn read_file(
    file: *mut c_void,
    buffer: *mut c_void,
    bytes: u32,
    read: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    /// `ERROR_IO_PENDING`.
    const PENDING: u32 = 997;
    let memory = crate::xmem::memory();
    let ours = memory.holds(buffer as u64);
    if ours {
        memory.will_write(buffer as u64, u64::from(bytes));
    }
    // SAFETY: the title's own arguments, passed on.
    let result = unsafe { sys::ReadFile(file, buffer, bytes, read, overlapped) };
    // SAFETY: reading the calling thread's last error.
    if ours && result == 0 && !overlapped.is_null() && unsafe { sys::GetLastError() } == PENDING {
        let mut transferred = 0u32;
        // SAFETY: the same file and overlapped structure, waited for; then the error the
        // title expects is put back.
        unsafe {
            sys::GetOverlappedResult(file, overlapped, &mut transferred, 1);
            sys::SetLastError(PENDING);
        }
    }
    result
}

pub(crate) fn register(imports: &mut Imports) {
    const STDIO: &str = "api-ms-win-crt-stdio-l1-1-0.dll";
    let functions: [(&str, &str, usize); 7] = [
        ("kernel32.dll", "ReadFile", read_file as *const () as usize),
        (
            "kernel32.dll",
            "CreateFileW",
            create_file_w as *const () as usize,
        ),
        (
            "kernel32.dll",
            "GetFileAttributesW",
            get_file_attributes_w as *const () as usize,
        ),
        (
            "kernel32.dll",
            "GetFileAttributesExW",
            get_file_attributes_ex_w as *const () as usize,
        ),
        (
            "kernel32.dll",
            "FindFirstFileW",
            find_first_file_w as *const () as usize,
        ),
        (STDIO, "fopen", fopen as *const () as usize),
        (STDIO, "_wfopen", wfopen as *const () as usize),
    ];
    for (module, name, address) in functions {
        // SAFETY: each function has the signature of the function it stands in for.
        unsafe { imports.implement(module, Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_opened_buffered_whatever_the_title_asks() {
        // Unbuffered and overlapped: overlapped stays.
        assert_eq!(host_flags(0x6000_0000), 0x4000_0000);
        assert_eq!(host_flags(NO_BUFFERING), 0);
        assert_eq!(host_flags(0x0800_0080), 0x0800_0080);
    }

    #[test]
    fn the_scratch_drive_leads_into_its_folder() {
        let scratch = Path::new(r"C:\data\scratch");
        assert_eq!(
            lead(r"t:\telemetrycache.dat", None, Some(scratch)),
            (r"C:\data\scratch\telemetrycache.dat".to_owned(), false)
        );
        assert_eq!(
            lead("T:/a/b.bin", None, Some(scratch)),
            (r"C:\data\scratch\a/b.bin".to_owned(), false)
        );
        // A name that only starts like the drive is not on it.
        assert_eq!(on_drive("tools/x", b't'), None);
        assert_eq!(on_drive(r"T:\x", b't'), Some("x"));
        assert_eq!(on_drive("T:", b't'), Some(""));
    }

    #[test]
    fn the_package_drive_leads_into_the_game_folder_and_nothing_else_does() {
        let game = Path::new(r"D:\games\title");
        assert_eq!(
            lead(r"G:\appdata0.rpf", Some(game), None),
            (r"D:\games\title\appdata0.rpf".to_owned(), true)
        );
        assert_eq!(
            lead("g:/data/x.bin", Some(game), None),
            (r"D:\games\title\data/x.bin".to_owned(), true)
        );
        assert_eq!(
            lead("G:", Some(game), None),
            (r"D:\games\title\".to_owned(), true)
        );
        assert_eq!(
            lead(r"T:\save.bin", Some(game), None),
            (r"T:\save.bin".to_owned(), false)
        );
        assert_eq!(
            lead(r"Game:\x", Some(game), None),
            (r"Game:\x".to_owned(), false)
        );
        // With no title set, nothing is led anywhere.
        assert_eq!(lead(r"G:\x", None, None), (r"G:\x".to_owned(), false));
    }
}
