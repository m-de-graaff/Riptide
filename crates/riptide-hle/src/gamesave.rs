//! The runtime's save area (`XGameSave`): a title's saved games.
//!
//! On the console saves are containers of named blobs, kept on the machine and copied to
//! the service. Here they are folders under
//! `%LOCALAPPDATA%\Riptide\titles\<package identity>\saves\<configuration>`, on this
//! machine only.
//!
//! Built so far: opening and closing the provider for a user, and the quota left. Reading,
//! listing and writing containers and blobs name themselves when called; they are written
//! with `saves.rs`, which never leaves a half-written save.

use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};

use crate::package::{attribute, config};
use crate::trace::trace;
use crate::xasync::{self, AsyncBlock};

/// The interface id a title asks for: `1bfff3af-f14a-40a3-8e35-9ada906593f9`.
pub(crate) const INTERFACE: [u8; 16] = [
    0xaf, 0xf3, 0xff, 0x1b, 0x4a, 0xf1, 0xa3, 0x40, 0x8e, 0x35, 0x9a, 0xda, 0x90, 0x65, 0x93, 0xf9,
];

pub(crate) const NAME: &str = "saves";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_FAIL: i32 = 0x8000_4005_u32 as i32;

/// What a user's saves for one title may take in all, as the console documents it.
const QUOTA: u64 = 256 << 20;

/// An open save provider: where its containers lie.
struct Provider {
    root: PathBuf,
}

/// A configuration id as a folder name: letters, digits, `-` and `_` stay, anything else
/// becomes `_`, so that the name can neither leave the saves folder nor be empty.
fn folder_name(configuration: &str) -> String {
    let name: String = configuration
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => character,
            _ => '_',
        })
        .take(100)
        .collect();
    if name.is_empty() {
        "default".to_owned()
    } else {
        name
    }
}

/// Where the saves of the configuration `configuration` lie under a title's data folder.
fn root_in(title_data: &Path, configuration: &str) -> PathBuf {
    title_data.join("saves").join(folder_name(configuration))
}

/// This title's data folder on this machine.
fn title_data() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let identity = config().and_then(|config| attribute(config, "Identity", "Name"));
    Some(crate::localstorage::title_data(
        Path::new(&base),
        identity.unwrap_or("unknown"),
    ))
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

/// What names an open-provider call's result.
fn initialize_identity() -> usize {
    initialize_async as *const () as usize
}

/// +0x20 `XGameSaveInitializeProviderAsync`: `(user, configuration id, whether to sync on
/// demand, block)`. The saves folder is made and the provider is open at once: there is
/// nothing to fetch from a service.
///
/// # Safety
/// `configuration` is null or a null-terminated string; `block` is null or a block that
/// stays valid until the result is taken.
unsafe extern "system" fn initialize_async(
    _this: usize,
    _user: *const c_void,
    configuration: *const c_char,
    _sync_on_demand: bool,
    block: *mut AsyncBlock,
) -> i32 {
    if configuration.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a null-terminated string of the title's.
    let configuration = unsafe { CStr::from_ptr(configuration) }.to_string_lossy();
    let opened = title_data()
        .map(|data| root_in(&data, &configuration))
        .ok_or(E_FAIL)
        .and_then(|root| match std::fs::create_dir_all(&root) {
            Ok(()) => Ok(root),
            Err(error) => {
                trace!("saves: {} cannot be made: {error}", root.display());
                Err(E_FAIL)
            }
        });
    let outcome = opened.map(|root| {
        trace!(
            "saves for configuration {configuration} are in {}",
            root.display()
        );
        let provider = Box::into_raw(Box::new(Provider { root }));
        (provider as usize).to_ne_bytes().to_vec()
    });
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, initialize_identity(), outcome) }
}

/// +0x28 `XGameSaveInitializeProviderResult`: `(block, provider out)`.
///
/// # Safety
/// `block` is null or the block of an open-provider call; `out` is null or where the
/// provider goes.
unsafe extern "system" fn initialize_result(
    _this: usize,
    block: *mut AsyncBlock,
    out: *mut *mut Provider,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's block, and room for one handle.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            initialize_identity() as *const c_void,
            size_of::<*mut Provider>(),
            out.cast(),
            std::ptr::null_mut(),
        )
    }
}

/// +0x30 `XGameSaveCloseProvider`.
///
/// # Safety
/// `provider` is null or one from an open-provider call, closed once.
unsafe extern "system" fn close_provider(_this: usize, provider: *mut Provider) {
    if !provider.is_null() {
        // SAFETY: made by `Box::into_raw` in `initialize_async`.
        drop(unsafe { Box::from_raw(provider) });
    }
}

/// +0x38 `XGameSaveGetRemainingQuota`: `(provider, bytes left out)`.
///
/// # Safety
/// `provider` is null or an open provider; `out` is null or where the count goes.
unsafe extern "system" fn remaining_quota(
    _this: usize,
    provider: *const Provider,
    out: *mut i64,
) -> i32 {
    // SAFETY: the title's pointers; null is refused.
    let (Some(provider), Some(out)) = (unsafe { (provider.as_ref(), out.as_mut()) }) else {
        return E_INVALIDARG;
    };
    *out = QUOTA.saturating_sub(bytes_in(&provider.root)) as i64;
    S_OK
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (4, initialize_async as *const () as usize),
        (5, initialize_result as *const () as usize),
        (6, close_provider as *const () as usize),
        (7, remaining_quota as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configuration_id_cannot_leave_the_saves_folder() {
        assert_eq!(
            folder_name("00000000-0000-0000-0000-00007952987a"),
            "00000000-0000-0000-0000-00007952987a"
        );
        assert_eq!(folder_name(r"..\..\Windows"), "______Windows");
        assert_eq!(folder_name(""), "default");
        let root = root_in(Path::new(r"C:\data"), "a/b");
        assert_eq!(root, Path::new(r"C:\data\saves\a_b"));
    }

    #[test]
    fn the_quota_left_is_the_quota_less_what_the_saves_hold() {
        let folder = std::env::temp_dir().join(format!("riptide-quota-{}", std::process::id()));
        std::fs::create_dir_all(folder.join("container")).unwrap();
        std::fs::write(folder.join("container").join("blob"), [0u8; 1000]).unwrap();
        let provider = Provider {
            root: folder.clone(),
        };
        let mut left = 0;
        // SAFETY: a live provider and a valid out-pointer, and the null ones under test.
        unsafe {
            assert_eq!(remaining_quota(0, &provider, &mut left), S_OK);
            assert_eq!(
                remaining_quota(0, std::ptr::null(), &mut left),
                E_INVALIDARG
            );
        }
        assert_eq!(left, (QUOTA - 1000) as i64);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
