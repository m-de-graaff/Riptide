//! The title's file calls.
//!
//! A title names its files by the console's drives. `G:` is the title's own package, which
//! here is the game folder. The package is read-only on the console, and it is here: a call
//! that would write to it, or create or delete anything in it, is refused, so a title can
//! never change the game's files.
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

/// Where a path the title gave leads: the text to pass to Windows, and whether it lies in
/// the title's package.
fn lead(path: &str, package: Option<&Path>) -> (String, bool) {
    let bytes = path.as_bytes();
    let on_g = bytes.len() >= 2
        && bytes[0].eq_ignore_ascii_case(&b'g')
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'\\' || bytes[2] == b'/');
    match (on_g, package) {
        (true, Some(package)) => {
            let rest = path[2..].trim_start_matches(['\\', '/']);
            (package.join(rest).to_string_lossy().into_owned(), true)
        }
        _ => (path.to_owned(), false),
    }
}

/// [`lead`] for the title that is running, as a null-terminated UTF-16 string.
pub(crate) fn lead_wide(path: &str) -> (Vec<u16>, bool) {
    let (led, in_package) = lead(path, crate::process::title_folder().as_deref());
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
            flags,
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

pub(crate) fn register(imports: &mut Imports) {
    const STDIO: &str = "api-ms-win-crt-stdio-l1-1-0.dll";
    let functions: [(&str, &str, usize); 6] = [
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
    fn the_package_drive_leads_into_the_game_folder_and_nothing_else_does() {
        let game = Path::new(r"D:\games\title");
        assert_eq!(
            lead(r"G:\appdata0.rpf", Some(game)),
            (r"D:\games\title\appdata0.rpf".to_owned(), true)
        );
        assert_eq!(
            lead("g:/data/x.bin", Some(game)),
            (r"D:\games\title\data/x.bin".to_owned(), true)
        );
        assert_eq!(
            lead("G:", Some(game)),
            (r"D:\games\title\".to_owned(), true)
        );
        assert_eq!(
            lead(r"T:\save.bin", Some(game)),
            (r"T:\save.bin".to_owned(), false)
        );
        assert_eq!(lead(r"Game:\x", Some(game)), (r"Game:\x".to_owned(), false));
        // With no title set, nothing is led anywhere.
        assert_eq!(lead(r"G:\x", None), (r"G:\x".to_owned(), false));
    }
}
