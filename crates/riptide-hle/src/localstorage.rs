//! The runtime's persistent local storage area (`XPersistentLocalStorage`): a folder of the
//! title's own on the machine, which it reads and writes with ordinary file calls and which
//! is still there the next time it runs.
//!
//! The folder is `%LOCALAPPDATA%\Riptide\titles\<package identity>\local-storage`, made the
//! first time the title asks for it. It is never in the game folder, which is read-only.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::package::{attribute, config, copy_text, element};
use crate::trace::trace;

/// The interface id a title asks for: `41a4e10c-5a7e-41d9-8c37-37bde62a07d6`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x0c, 0xe1, 0xa4, 0x41, 0x7e, 0x5a, 0xd9, 0x41, 0x8c, 0x37, 0x37, 0xbd, 0xe6, 0x2a, 0x07, 0xd6,
];

pub(crate) const NAME: &str = "persistent local storage";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
/// `ERROR_PATH_NOT_FOUND` as a result code.
const E_PATH_NOT_FOUND: i32 = 0x8007_0003_u32 as i32;

/// What a title gets when its package declares no size: the console's smallest.
const DEFAULT_BYTES: u64 = 64 << 20;

/// A package identity as a folder name: anything Windows refuses in a name becomes `_`.
fn folder_name(identity: &str) -> String {
    let name: String = identity
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let name = name.trim_matches([' ', '.']);
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name.to_owned()
    }
}

/// Where a title's data on this machine lies, under the folder for local application data.
pub(crate) fn title_data(local_app_data: &Path, identity: &str) -> PathBuf {
    local_app_data
        .join("Riptide")
        .join("titles")
        .join(folder_name(identity))
}

/// The folder, made if it was not there. `None` when it cannot be made.
fn folder() -> Option<&'static Path> {
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let base = std::env::var_os("LOCALAPPDATA")?;
            let identity = config().and_then(|config| attribute(config, "Identity", "Name"));
            let folder =
                title_data(Path::new(&base), identity.unwrap_or("unknown")).join("local-storage");
            match std::fs::create_dir_all(&folder) {
                Ok(()) => {
                    trace!("persistent local storage: {}", folder.display());
                    Some(folder)
                }
                Err(error) => {
                    trace!(
                        "persistent local storage: {} cannot be made: {error}",
                        folder.display()
                    );
                    None
                }
            }
        })
        .as_deref()
}

/// The bytes the package declares for its local storage (`PersistentLocalStorage/SizeMB`).
fn declared_bytes(config: Option<&str>) -> u64 {
    config
        .and_then(|config| element(config, "PersistentLocalStorage"))
        .and_then(|inner| element(inner, "SizeMB"))
        .and_then(|size| size.parse::<u64>().ok())
        .map_or(DEFAULT_BYTES, |mib| mib << 20)
}

/// The bytes of every file under `folder`.
fn bytes_in(folder: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => bytes_in(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

/// `XPersistentLocalStorageGetPathSize`: the bytes the path needs, with its terminator.
///
/// # Safety
/// `size` is null or where the size goes.
unsafe extern "system" fn path_size(_this: usize, size: *mut usize) -> i32 {
    if size.is_null() {
        return E_INVALIDARG;
    }
    let Some(folder) = folder() else {
        return E_PATH_NOT_FOUND;
    };
    // SAFETY: checked non-null.
    unsafe { size.write(folder.to_string_lossy().len() + 1) };
    S_OK
}

/// `XPersistentLocalStorageGetPath`: the folder's path, as UTF-8: `(buffer size, buffer,
/// bytes used out)`.
///
/// # Safety
/// `path` is null or has room for `size` bytes; `used` is null or where the count goes.
unsafe extern "system" fn path(_this: usize, size: usize, path: *mut u8, used: *mut usize) -> i32 {
    let Some(folder) = folder() else {
        return E_PATH_NOT_FOUND;
    };
    let text = folder.to_string_lossy();
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(text.len() + 1) };
    }
    // SAFETY: the title's buffer, as it describes it.
    let result = unsafe { copy_text(&text, size, path) };
    trace!(
        "XPersistentLocalStorageGetPath = {text} ({:#010x})",
        result as u32
    );
    result
}

/// `XPersistentLocalStorageSpaceInfo`.
#[repr(C)]
struct SpaceInfo {
    available_free_bytes: u64,
    total_free_bytes: u64,
}

/// `XPersistentLocalStorageGetSpaceInfo`: what is left of the size the package declares,
/// after the files that are in the folder now.
///
/// # Safety
/// `info` is null or where the answer goes.
unsafe extern "system" fn space_info(_this: usize, info: *mut SpaceInfo) -> i32 {
    if info.is_null() {
        return E_INVALIDARG;
    }
    let Some(folder) = folder() else {
        return E_PATH_NOT_FOUND;
    };
    let free = declared_bytes(config()).saturating_sub(bytes_in(folder));
    trace!("XPersistentLocalStorageGetSpaceInfo: {free} bytes free");
    // SAFETY: checked non-null.
    unsafe {
        info.write(SpaceInfo {
            available_free_bytes: free,
            total_free_bytes: free,
        })
    };
    S_OK
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        // +0x18 `XPersistentLocalStorageGetPathSize`.
        (3, path_size as *const () as usize),
        // +0x20 `XPersistentLocalStorageGetPath`.
        (4, path as *const () as usize),
        // +0x28 `XPersistentLocalStorageGetSpaceInfo`.
        (5, space_info as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_titles_data_lies_under_its_identity() {
        assert_eq!(
            title_data(Path::new(r"C:\Users\x\AppData\Local"), "Maker.Game"),
            Path::new(r"C:\Users\x\AppData\Local\Riptide\titles\Maker.Game")
        );
    }

    #[test]
    fn an_identity_cannot_leave_its_folder() {
        assert_eq!(folder_name(r"..\..\Windows"), "_.._Windows");
        assert_eq!(folder_name("a:b*c"), "a_b_c");
        assert_eq!(folder_name(".."), "unknown");
        assert_eq!(folder_name(""), "unknown");
    }

    #[test]
    fn the_declared_size_comes_from_the_configuration() {
        let config = "<Game><PersistentLocalStorage>\n <SizeMB>2048</SizeMB>\n\
                      </PersistentLocalStorage></Game>";
        assert_eq!(declared_bytes(Some(config)), 2048 << 20);
        assert_eq!(declared_bytes(Some("<Game />")), DEFAULT_BYTES);
        assert_eq!(declared_bytes(None), DEFAULT_BYTES);
    }

    #[test]
    fn the_bytes_in_a_folder_count_its_subfolders() {
        let folder = std::env::temp_dir().join(format!("riptide-pls-{}", std::process::id()));
        std::fs::create_dir_all(folder.join("inner")).unwrap();
        std::fs::write(folder.join("a"), [0u8; 10]).unwrap();
        std::fs::write(folder.join("inner").join("b"), [0u8; 5]).unwrap();
        assert_eq!(bytes_in(&folder), 15);
        std::fs::remove_dir_all(&folder).unwrap();
        assert_eq!(bytes_in(&folder), 0);
    }
}
