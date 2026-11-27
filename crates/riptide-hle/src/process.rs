//! Who the title is told it is.
//!
//! The title runs inside Riptide's process, so Windows would answer its questions about "this
//! program" with Riptide's own image, file name and command line. A title uses those answers
//! to find its files and its runtime, so they are answered as the title here: its own mapped
//! image, its own path, and a command line with no arguments.

use std::ffi::{CString, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::sys;

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

struct Title {
    base: usize,
    /// The image's full path, null-terminated, as UTF-16 and in the ANSI code page's place
    /// as UTF-8 bytes.
    path_wide: Vec<u16>,
    path_narrow: CString,
    /// The command line: the quoted path and nothing else.
    command_line: CString,
    /// What the C runtime's `__p___argc` and `__p___argv` point at.
    argc: i32,
    argv: Argv,
}

/// A null-terminated argument array and the pointer to it that `__p___argv` hands out.
struct Argv {
    /// Points at the leaked array `[path, null]`.
    array: *mut *mut c_char,
}

// SAFETY: the array is leaked and never written after it is made.
unsafe impl Send for Argv {}
// SAFETY: as above.
unsafe impl Sync for Argv {}

static TITLE: OnceLock<Title> = OnceLock::new();

/// Tells this module which image is the title. Called once, after the title is loaded and
/// before it runs. Until then every question is passed to Windows unchanged.
pub fn set_title(base: usize, path: &Path) {
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let text = path.to_string_lossy().replace('\0', "");
    let path_narrow = CString::new(text.clone()).expect("no null bytes");
    let command_line = CString::new(format!("\"{text}\"")).expect("no null bytes");
    let array: &'static mut [*mut c_char; 2] = Box::leak(Box::new([
        path_narrow.clone().into_raw(),
        std::ptr::null_mut(),
    ]));
    let _ = TITLE.set(Title {
        base,
        path_wide,
        path_narrow,
        command_line,
        argc: 1,
        argv: Argv {
            array: array.as_mut_ptr(),
        },
    });
}

/// Whether `module` means the title: null, which asks for "this program", or its base.
fn is_title(title: &Title, module: *mut c_void) -> bool {
    module.is_null() || module as usize == title.base
}

/// The title's `GetModuleHandleW`: a null name is the title's own image.
///
/// # Safety
/// `name` is null or a null-terminated UTF-16 string, as the title must pass.
pub unsafe extern "system" fn get_module_handle_w(name: *const u16) -> *mut c_void {
    match TITLE.get() {
        Some(title) if name.is_null() => title.base as *mut c_void,
        // SAFETY: passed through as received.
        _ => unsafe { sys::GetModuleHandleW(name) },
    }
}

/// The title's `GetModuleHandleA`.
///
/// # Safety
/// `name` is null or a null-terminated string.
pub unsafe extern "system" fn get_module_handle_a(name: *const u8) -> *mut c_void {
    match TITLE.get() {
        Some(title) if name.is_null() => title.base as *mut c_void,
        // SAFETY: passed through as received.
        _ => unsafe { sys::GetModuleHandleA(name) },
    }
}

/// Copies `text` (without its terminator) into a buffer of `size` units as Windows'
/// `GetModuleFileName` does: the count copied, or a truncated copy and `size` with
/// `ERROR_INSUFFICIENT_BUFFER` if it does not fit.
///
/// # Safety
/// `buffer` is valid for `size` units.
unsafe fn copy_out<T: Copy + Default>(text: &[T], buffer: *mut T, size: u32) -> u32 {
    if buffer.is_null() || size == 0 {
        return 0;
    }
    let room = size as usize;
    // SAFETY: the caller vouches for `size` units at `buffer`.
    let out = unsafe { std::slice::from_raw_parts_mut(buffer, room) };
    if text.len() < room {
        out[..text.len()].copy_from_slice(text);
        out[text.len()] = T::default();
        text.len() as u32
    } else {
        out[..room - 1].copy_from_slice(&text[..room - 1]);
        out[room - 1] = T::default();
        // SAFETY: the call has no preconditions.
        unsafe { sys::SetLastError(ERROR_INSUFFICIENT_BUFFER) };
        size
    }
}

/// The title's `GetModuleFileNameW`: the title's own path for itself.
///
/// # Safety
/// `buffer` is valid for `size` UTF-16 units.
pub unsafe extern "system" fn get_module_file_name_w(
    module: *mut c_void,
    buffer: *mut u16,
    size: u32,
) -> u32 {
    match TITLE.get() {
        Some(title) if is_title(title, module) => {
            let path = &title.path_wide[..title.path_wide.len() - 1];
            // SAFETY: the caller's buffer, as received.
            unsafe { copy_out(path, buffer, size) }
        }
        // SAFETY: passed through as received.
        _ => unsafe { sys::GetModuleFileNameW(module, buffer, size) },
    }
}

/// The title's `GetModuleFileNameA`.
///
/// # Safety
/// `buffer` is valid for `size` bytes.
pub unsafe extern "system" fn get_module_file_name_a(
    module: *mut c_void,
    buffer: *mut u8,
    size: u32,
) -> u32 {
    match TITLE.get() {
        Some(title) if is_title(title, module) => {
            // SAFETY: the caller's buffer, as received.
            unsafe { copy_out(title.path_narrow.as_bytes(), buffer, size) }
        }
        // SAFETY: passed through as received.
        _ => unsafe { sys::GetModuleFileNameA(module, buffer, size) },
    }
}

/// The title's `GetCommandLineA`: its own path, quoted, with no arguments.
pub extern "system" fn get_command_line_a() -> *const c_char {
    match TITLE.get() {
        Some(title) => title.command_line.as_ptr(),
        // SAFETY: the call has no preconditions.
        None => unsafe { sys::GetCommandLineA() },
    }
}

/// The C runtime's `__p___argc`: where the argument count is kept.
pub extern "C" fn p_argc() -> *const i32 {
    static NONE: i32 = 0;
    TITLE
        .get()
        .map_or(&raw const NONE, |title| &raw const title.argc)
}

/// The C runtime's `__p___argv`: where the pointer to the argument array is kept.
pub extern "C" fn p_argv() -> *const *mut *mut c_char {
    static NONE: Argv = Argv {
        array: std::ptr::null_mut(),
    };
    &raw const TITLE.get().map_or(&NONE, |title| &title.argv).array
}

/// The C runtime's `_get_narrow_winmain_command_line`: the arguments after the program's
/// name, of which there are none.
pub extern "C" fn narrow_winmain_command_line() -> *const c_char {
    c"".as_ptr()
}

pub(crate) fn register(imports: &mut Imports) {
    const RUNTIME: &str = "api-ms-win-crt-runtime-l1-1-0.dll";
    let functions: [(&str, &str, usize); 8] = [
        (
            "kernel32.dll",
            "GetModuleHandleW",
            get_module_handle_w as *const () as usize,
        ),
        (
            "kernel32.dll",
            "GetModuleHandleA",
            get_module_handle_a as *const () as usize,
        ),
        (
            "kernel32.dll",
            "GetModuleFileNameW",
            get_module_file_name_w as *const () as usize,
        ),
        (
            "kernel32.dll",
            "GetModuleFileNameA",
            get_module_file_name_a as *const () as usize,
        ),
        (
            "kernel32.dll",
            "GetCommandLineA",
            get_command_line_a as *const () as usize,
        ),
        (RUNTIME, "__p___argc", p_argc as *const () as usize),
        (RUNTIME, "__p___argv", p_argv as *const () as usize),
        (
            RUNTIME,
            "_get_narrow_winmain_command_line",
            narrow_winmain_command_line as *const () as usize,
        ),
    ];
    for (module, name, address) in functions {
        // SAFETY: each function above has the signature of the Windows or C runtime function
        // it is registered as.
        unsafe { imports.implement(module, Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_copied_out_whole_or_cut_short_as_windows_does() {
        let text: Vec<u16> = "C:\\game\\title.exe".encode_utf16().collect();
        let mut roomy = [0xFFFFu16; 64];
        // SAFETY: the buffers are as long as the sizes passed.
        let copied = unsafe { copy_out(&text, roomy.as_mut_ptr(), 64) };
        assert_eq!(copied as usize, text.len());
        assert_eq!(&roomy[..text.len()], &text[..]);
        assert_eq!(roomy[text.len()], 0);

        let mut tight = [0xFFFFu16; 8];
        // SAFETY: as above.
        let copied = unsafe { copy_out(&text, tight.as_mut_ptr(), 8) };
        assert_eq!(copied, 8);
        assert_eq!(&tight[..7], &text[..7]);
        assert_eq!(tight[7], 0);

        // SAFETY: a null buffer is never written.
        assert_eq!(unsafe { copy_out(&text, std::ptr::null_mut(), 8) }, 0);
    }

    #[test]
    fn before_a_title_is_set_windows_answers() {
        // No test in this crate sets a title, so the host's answers come through.
        if TITLE.get().is_none() {
            // SAFETY: a null name asks for this program's own image.
            let own = unsafe { get_module_handle_w(std::ptr::null()) };
            // SAFETY: as above.
            assert_eq!(own, unsafe { sys::GetModuleHandleW(std::ptr::null()) });
        }
    }
}
