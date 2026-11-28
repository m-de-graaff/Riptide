//! The console's runtime services, `xgameruntime.dll`: users, task queues, saves, packages
//! and the rest of what a title asks the system for.
//!
//! A title does not import this module. A library linked into the title loads it by name
//! when the title starts, looks up six functions, and from then on asks `QueryApiImpl` for
//! each area of the API as an object. So the module is served from here: loading it by name
//! gives a handle that stands for it, and looking up its functions through that handle gives
//! Riptide's.
//!
//! No area is built yet. Each one asked for is handed out as an object whose every method
//! names the area and its position when called, so the first method a title needs is found
//! by name.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;

use riptide_pe::Symbol;

use crate::com;
use crate::dstorage::guid;
use crate::imports::Imports;
use crate::sys;
use crate::trace::trace;

const S_OK: i32 = 0;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;

/// More methods than any area of the runtime has.
const AREA_SLOTS: usize = 96;

/// What the handle for the runtime module points at. Only its address matters.
static MODULE: u8 = 0;

fn module_handle() -> *mut c_void {
    (&raw const MODULE).cast_mut().cast()
}

/// Whether a module name, with or without a path, is the runtime's.
fn is_runtime(name: &str) -> bool {
    let file = name.rsplit(['\\', '/']).next().unwrap_or(name);
    file.eq_ignore_ascii_case("xgameruntime.dll") || file.eq_ignore_ascii_case("xgameruntime")
}

/// A null-terminated UTF-16 string as text.
///
/// # Safety
/// `text` is null or null-terminated.
unsafe fn wide(text: *const u16) -> String {
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
unsafe fn narrow(text: *const u8) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the caller vouches for the terminator.
    unsafe { CStr::from_ptr(text.cast()) }
        .to_string_lossy()
        .into_owned()
}

/// The title's `LoadLibraryExW`: the runtime module is served from here, anything else is
/// Windows' business.
///
/// # Safety
/// `name` is a null-terminated UTF-16 string, as the title must pass.
pub unsafe extern "system" fn load_library_ex_w(
    name: *const u16,
    file: *mut c_void,
    flags: u32,
) -> *mut c_void {
    // SAFETY: the title's string.
    let text = unsafe { wide(name) };
    if is_runtime(&text) {
        trace!("LoadLibraryExW({text}): the runtime services");
        return module_handle();
    }
    // SAFETY: passed through as received.
    let module = unsafe { sys::LoadLibraryExW(name, file, flags) };
    trace!("LoadLibraryExW({text}, flags {flags:#x}) = {module:p}");
    module
}

/// The title's `LoadLibraryW`.
///
/// # Safety
/// As [`load_library_ex_w`].
pub unsafe extern "system" fn load_library_w(name: *const u16) -> *mut c_void {
    // SAFETY: as received.
    unsafe { load_library_ex_w(name, std::ptr::null_mut(), 0) }
}

/// The title's `LoadLibraryExA`.
///
/// # Safety
/// `name` is a null-terminated string.
pub unsafe extern "system" fn load_library_ex_a(
    name: *const u8,
    file: *mut c_void,
    flags: u32,
) -> *mut c_void {
    // SAFETY: the title's string.
    let text = unsafe { narrow(name) };
    if is_runtime(&text) {
        trace!("LoadLibraryExA({text}): the runtime services");
        return module_handle();
    }
    // SAFETY: passed through as received.
    let module = unsafe { sys::LoadLibraryExA(name, file, flags) };
    trace!("LoadLibraryExA({text}, flags {flags:#x}) = {module:p}");
    module
}

/// The title's `FreeLibrary`: the runtime module is never unloaded.
///
/// # Safety
/// `module` is a module handle the title holds.
pub unsafe extern "system" fn free_library(module: *mut c_void) -> i32 {
    if module == module_handle() {
        return 1;
    }
    // SAFETY: passed through as received.
    unsafe { sys::FreeLibrary(module) }
}

/// The title's `GetProcAddress`: the runtime module's functions are Riptide's.
///
/// # Safety
/// `name` is a null-terminated string, or an ordinal below 65536 in its place.
pub unsafe extern "system" fn get_proc_address(module: *mut c_void, name: *const u8) -> usize {
    if module != module_handle() {
        // SAFETY: passed through as received.
        return unsafe { sys::GetProcAddress(module, name) } as usize;
    }
    if (name as usize) < 0x1_0000 {
        return 0;
    }
    // SAFETY: not an ordinal, so a string.
    let text = unsafe { narrow(name) };
    let address = match text.as_str() {
        "QueryApiImpl" => query_api_impl as *const () as usize,
        "InitializeApiImpl" => initialize_api_impl as *const () as usize,
        "InitializeApiImplEx" => initialize_api_impl_ex as *const () as usize,
        "InitializeApiImplEx2" => initialize_api_impl_ex2 as *const () as usize,
        "UninitializeApiImpl" => uninitialize_api_impl as *const () as usize,
        "XErrorReport" => error_report as *const () as usize,
        _ => 0,
    };
    trace!(
        "runtime services: {text} {}",
        if address == 0 {
            "is not a function of theirs"
        } else {
            "looked up"
        }
    );
    address
}

/// `InitializeApiImpl`: the two numbers name the build of the library in the title.
extern "system" fn initialize_api_impl(version: u64, build: u64) -> i32 {
    trace!("runtime services: initialised by library {version:#x}, {build:#x}");
    S_OK
}

/// `InitializeApiImplEx`.
extern "system" fn initialize_api_impl_ex(version: u64, build: u64, flags: u64) -> i32 {
    trace!("runtime services: initialised by library {version:#x}, {build:#x}, flags {flags:#x}");
    S_OK
}

/// `InitializeApiImplEx2`: the one a title's library prefers when it is there.
extern "system" fn initialize_api_impl_ex2(
    version: u64,
    build: u64,
    flags: u64,
    options: *const c_void,
) -> i32 {
    trace!(
        "runtime services: initialised by library {version:#x}, {build:#x}, flags {flags:#x}, \
         options {options:p}"
    );
    S_OK
}

/// `UninitializeApiImpl`.
extern "system" fn uninitialize_api_impl() {
    trace!("runtime services: uninitialised");
}

/// `XErrorReport`: the title's library reporting an error of its own.
///
/// # Safety
/// `message` is null or a null-terminated string.
unsafe extern "system" fn error_report(result: i32, message: *const c_char) {
    // SAFETY: the title's string.
    let message = unsafe { narrow(message.cast()) };
    trace!(
        "runtime services: the title reports error {:#010x}: {message}",
        result as u32
    );
}

/// The areas handed out so far, by provider and interface id.
static AREAS: Mutex<Vec<Area>> = Mutex::new(Vec::new());

/// A provider id, an interface id, and the object handed out for them.
type Area = ([u8; 16], [u8; 16], usize);

/// `QueryApiImpl`: one area of the runtime's API as an object. None is built yet, so each
/// is an object whose methods name themselves when called.
///
/// # Safety
/// The ids are null or point at 16 bytes each; `out` is null or where the object goes.
unsafe extern "system" fn query_api_impl(
    provider: *const [u8; 16],
    interface: *const [u8; 16],
    out: *mut usize,
) -> i32 {
    // SAFETY: the title's ids; null is refused.
    let (Some(&provider), Some(&interface)) =
        (unsafe { provider.as_ref() }, unsafe { interface.as_ref() })
    else {
        return E_POINTER;
    };
    if out.is_null() {
        return E_POINTER;
    }
    let mut areas = AREAS.lock().unwrap_or_else(|poison| poison.into_inner());
    let known = areas
        .iter()
        .find(|(p, i, _)| (*p, *i) == (provider, interface))
        .map(|&(_, _, object)| object);
    let object = known.unwrap_or_else(|| {
        // The areas with methods built, by interface id.
        let built = if interface == crate::taskqueue::INTERFACE {
            Some((crate::taskqueue::NAME, crate::taskqueue::methods()))
        } else if interface == crate::network::INTERFACE {
            Some((crate::network::NAME, crate::network::methods()))
        } else if interface == crate::user::INTERFACE {
            Some((crate::user::NAME, crate::user::methods()))
        } else if interface == crate::package::INTERFACE {
            Some((crate::package::NAME, crate::package::methods()))
        } else if interface == crate::localstorage::INTERFACE {
            Some((crate::localstorage::NAME, crate::localstorage::methods()))
        } else if interface == crate::display::INTERFACE {
            Some((crate::display::NAME, crate::display::methods()))
        } else if interface == crate::system::INTERFACE {
            Some((crate::system::NAME, crate::system::methods()))
        } else if interface == crate::gamesave::INTERFACE {
            Some((crate::gamesave::NAME, crate::gamesave::methods()))
        } else if interface == crate::user::GAMERTAG_INTERFACE {
            Some((crate::user::GAMERTAG_NAME, crate::user::gamertag_methods()))
        } else if interface == crate::game::INTERFACE {
            Some((crate::game::NAME, crate::game::methods()))
        } else if interface == crate::accessibility::INTERFACE {
            Some((crate::accessibility::NAME, crate::accessibility::methods()))
        } else if interface == crate::store::INTERFACE || interface == crate::store::LATER_INTERFACE
        {
            Some((crate::store::NAME, crate::store::methods()))
        } else {
            None
        };
        let (name, mut methods) = match built {
            Some((name, methods)) => (format!("runtime area {name}"), methods),
            None => (
                format!("runtime area {}", guid(Some(&interface))),
                Vec::new(),
            ),
        };
        trace!(
            "runtime services: {name} asked for (interface {}, provider {}); {} of its \
             methods are built, the others name themselves",
            guid(Some(&interface)),
            guid(Some(&provider)),
            methods.len()
        );
        methods.push((1, com::keep as *const () as usize));
        methods.push((2, com::keep as *const () as usize));
        let object = com::singleton(&name, AREA_SLOTS, &methods);
        areas.push((provider, interface, object));
        object
    });
    // SAFETY: checked non-null; the title's own out-pointer.
    unsafe { out.write(object) };
    S_OK
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, usize); 5] = [
        ("LoadLibraryExW", load_library_ex_w as *const () as usize),
        ("LoadLibraryExA", load_library_ex_a as *const () as usize),
        ("LoadLibraryW", load_library_w as *const () as usize),
        ("FreeLibrary", free_library as *const () as usize),
        ("GetProcAddress", get_proc_address as *const () as usize),
    ];
    for (name, address) in functions {
        // SAFETY: each function has the signature of the Windows function it stands in for.
        unsafe { imports.implement("kernel32.dll", Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide_z(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn the_runtime_module_is_known_by_name_with_or_without_a_path() {
        assert!(is_runtime("xgameruntime.dll"));
        assert!(is_runtime(r"D:\game\XGameRuntime.dll"));
        assert!(!is_runtime("kernel32.dll"));
        assert!(!is_runtime(r"D:\xgameruntime.dll\other.dll"));
    }

    #[test]
    fn loading_the_runtime_and_looking_up_its_functions_gives_ours() {
        // SAFETY: null-terminated names, and handles this test got from the same functions.
        unsafe {
            let name = wide_z(r"D:\game\xgameruntime.dll");
            let module = load_library_ex_w(name.as_ptr(), std::ptr::null_mut(), 0x800);
            assert_eq!(module, module_handle());
            for function in [
                c"QueryApiImpl",
                c"InitializeApiImpl",
                c"InitializeApiImplEx",
                c"InitializeApiImplEx2",
                c"UninitializeApiImpl",
                c"XErrorReport",
            ] {
                assert_ne!(get_proc_address(module, function.as_ptr().cast()), 0);
            }
            assert_eq!(get_proc_address(module, c"NotOne".as_ptr().cast()), 0);
            assert_eq!(free_library(module), 1);

            // Any other module is Windows' own.
            let kernel = wide_z("kernel32.dll");
            let host = load_library_w(kernel.as_ptr());
            assert!(!host.is_null() && host != module_handle());
            assert_ne!(get_proc_address(host, c"Sleep".as_ptr().cast()), 0);
        }
    }

    #[test]
    fn an_area_is_handed_out_once_and_the_same_each_time() {
        let (provider, interface) = ([1u8; 16], [2u8; 16]);
        let (mut first, mut second) = (0usize, 0usize);
        // SAFETY: valid ids and out-pointers.
        unsafe {
            assert_eq!(query_api_impl(&provider, &interface, &mut first), S_OK);
            assert_eq!(query_api_impl(&provider, &interface, &mut second), S_OK);
            assert_eq!(
                query_api_impl(std::ptr::null(), &interface, &mut second),
                E_POINTER
            );
        }
        assert!(first != 0 && first == second);
    }
}
