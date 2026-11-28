//! The runtime's save area (`XGameSave`): a title's saved games.
//!
//! On the console saves are containers of named blobs, kept on the machine and copied to
//! the service. Here they are kept under
//! `%LOCALAPPDATA%\Riptide\titles\<package identity>\saves\<configuration>`, on this
//! machine only, by the store in `gamesave/store.rs`. That store applies an update to a
//! container as one step, so a crash or a power cut in the middle of a save leaves the
//! previous save whole.
//!
//! Built: opening and closing a provider, the quota left, deleting a container, telling
//! about one container or listing them all or by the start of their names, container and
//! update handles, listing a container's blobs, reading blobs, and submitting an update.
//! Every asynchronous form does its work at once and completes its block before it returns:
//! there is no service to wait for, and nothing is ever out of sync with one.
//!
//! Not built: the three "files" methods at positions 30 to 32, which show the console's own
//! screens. They name themselves when called.
//!
//! What is known and what is assumed about the interface is said at each method. In short:
//! the structures, the error codes, the rules for a container's name, the quota and the
//! limit on one update are the public API's. Assumed are that a container's total size is
//! the sum of its blobs' bytes, that deleting a blob that is not there is no error, and
//! that reading from a container that does not exist answers "blob not found".

mod store;

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::package::{attribute, config};
use crate::trace::trace;
use crate::xasync::{self, AsyncBlock};
use store::{Change, NamedBlob, Snapshot};

/// The interface id a title asks for: `1bfff3af-f14a-40a3-8e35-9ada906593f9`.
pub(crate) const INTERFACE: [u8; 16] = [
    0xaf, 0xf3, 0xff, 0x1b, 0x4a, 0xf1, 0xa3, 0x40, 0x8e, 0x35, 0x9a, 0xda, 0x90, 0x65, 0x93, 0xf9,
];

pub(crate) const NAME: &str = "saves";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_FAIL: i32 = 0x8000_4005_u32 as i32;
/// What the asynchronous calls answer for a result buffer that is too small.
const E_NOT_SUFFICIENT_BUFFER: i32 = 0x8007_007a_u32 as i32;

// The save area's own error codes, as the public API names them.
/// `E_GS_INVALID_CONTAINER_NAME`: the name breaks the rules of [`valid_container_name`].
const INVALID_CONTAINER_NAME: i32 = 0x8083_0001_u32 as i32;
/// `E_GS_NO_ACCESS`: the saves cannot be read or written.
const NO_ACCESS: i32 = 0x8083_0002_u32 as i32;
/// `E_GS_OUT_OF_LOCAL_STORAGE`: the drive is full.
const OUT_OF_LOCAL_STORAGE: i32 = 0x8083_0003_u32 as i32;
/// `E_GS_UPDATE_TOO_BIG`: one update writes more than 16 MiB.
const UPDATE_TOO_BIG: i32 = 0x8083_0005_u32 as i32;
/// `E_GS_QUOTA_EXCEEDED`: the update would take the user's saves past the quota.
const QUOTA_EXCEEDED: i32 = 0x8083_0006_u32 as i32;
/// `E_GS_PROVIDED_BUFFER_TOO_SMALL`: the blobs read do not fit the title's buffer.
const BUFFER_TOO_SMALL: i32 = 0x8083_0007_u32 as i32;
/// `E_GS_BLOB_NOT_FOUND`: the container has no blob of a name that was asked for.
const BLOB_NOT_FOUND: i32 = 0x8083_0008_u32 as i32;

/// What a user's saves for one title may take in all, as the console documents it.
const QUOTA: u64 = 256 << 20;

/// The longest name a container may have, as the console documents it.
const LONGEST_CONTAINER_NAME: usize = 256;

/// An open save provider: where its containers lie and how much they may hold.
struct Provider {
    root: PathBuf,
    quota: u64,
}

/// A container handle. It holds only the container's name and where it would lie: the
/// container itself exists on disk once an update to it is submitted. It carries its own
/// copy of the provider's facts, so it stays usable if the title closes the provider first.
struct Container {
    root: PathBuf,
    quota: u64,
    name: String,
}

/// An update handle: the changes gathered so far, which touch nothing on disk until the
/// update is submitted.
struct Update {
    root: PathBuf,
    quota: u64,
    name: String,
    /// The container's new display name, or none to keep the one it has.
    display: Option<Vec<u8>>,
    changes: Mutex<Vec<Change>>,
}

impl Update {
    fn changes(&self) -> MutexGuard<'_, Vec<Change>> {
        self.changes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// A container as a title is told about it (`XGameSaveContainerInfo`).
#[repr(C)]
struct ContainerInfo {
    name: *const c_char,
    display_name: *const c_char,
    blob_count: u32,
    /// Assumed to be the bytes of the blobs alone, without their names.
    total_size: u64,
    /// Seconds since 1970 (the C `time_t`).
    last_modified: i64,
    /// Whether the service holds a newer copy. There is no service, so never.
    needs_sync: bool,
}

/// A blob as a title is told about it (`XGameSaveBlobInfo`).
#[repr(C)]
struct BlobInfo {
    name: *const c_char,
    size: u32,
}

/// A blob as a read hands it over (`XGameSaveBlob`). The name and the data lie in the
/// same buffer of the title's, after the array of these.
#[repr(C)]
struct Blob {
    info: BlobInfo,
    data: *mut u8,
}

const _: () = assert!(size_of::<ContainerInfo>() == 48 && size_of::<Blob>() == 24);

/// What a listing of containers calls for each one: `(info, context)`. Answering false
/// stops the listing. The answer is read as a byte, so that any value is safe to receive.
type ContainerCallback = unsafe extern "system" fn(*const ContainerInfo, *mut c_void) -> u8;

/// What a listing of blobs calls for each one: `(info, context)`. Answering false stops
/// the listing.
type BlobCallback = unsafe extern "system" fn(*const BlobInfo, *mut c_void) -> u8;

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

/// Whether a title may name a container `name`, by the console's documented rules: at most
/// 256 characters; the part up to the last `/` holds only letters, digits, `_` and `/`;
/// the part after it is not empty, holds only letters, digits, `_`, `.` and `-`, does not
/// end in `.` and has no two dots in a row.
///
/// The store keeps any name inside the provider's folder whatever it holds; these rules
/// are checked so that a title gets the error the console would give it.
fn valid_container_name(name: &str) -> bool {
    let (path, last) = match name.rfind('/') {
        Some(slash) => (&name[..slash], &name[slash + 1..]),
        None => ("", name),
    };
    let in_path = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'/';
    let in_last = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-');
    name.len() <= LONGEST_CONTAINER_NAME
        && path.bytes().all(in_path)
        && !last.is_empty()
        && last.bytes().all(in_last)
        && !last.ends_with('.')
        && !last.contains("..")
}

/// The bytes of a string of the title's, without the terminator.
///
/// # Safety
/// `text` is null or a null-terminated string.
unsafe fn bytes_at(text: *const c_char) -> Option<Vec<u8>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: a null-terminated string of the title's.
    Some(unsafe { CStr::from_ptr(text) }.to_bytes().to_vec())
}

/// A container name of the title's, or the error the console gives for it.
///
/// # Safety
/// `text` is null or a null-terminated string.
unsafe fn container_name_at(text: *const c_char) -> Result<String, i32> {
    // SAFETY: as received.
    let bytes = unsafe { bytes_at(text) }.ok_or(E_INVALIDARG)?;
    String::from_utf8(bytes)
        .ok()
        .filter(|name| valid_container_name(name))
        .ok_or(INVALID_CONTAINER_NAME)
}

/// Bytes that came from a null-terminated string, as one again.
fn c_text(bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap_or_default()
}

/// The code a title is given for a failure of the store, which is also written to the log
/// when it is the disk's doing and not the title's.
fn code(error: &store::Error, container: &str) -> i32 {
    match error {
        store::Error::Io(io) => {
            trace!("saves: container {container}: {io}");
            if io.kind() == std::io::ErrorKind::StorageFull {
                OUT_OF_LOCAL_STORAGE
            } else {
                NO_ACCESS
            }
        }
        store::Error::Damaged => {
            trace!("saves: container {container} does not read; its files are left as they are");
            NO_ACCESS
        }
        store::Error::Clash => {
            trace!("saves: container {container} shares its folder's hash with another");
            NO_ACCESS
        }
        store::Error::NoContainer | store::Error::NoBlob => BLOB_NOT_FOUND,
        store::Error::TooBig => UPDATE_TOO_BIG,
        store::Error::OverQuota => QUOTA_EXCEEDED,
    }
}

/// Opens a provider on the folder `root`, which is made if it is missing. What an
/// interrupted save left there is removed now.
fn open_at(root: PathBuf, quota: u64) -> Result<*mut Provider, i32> {
    if let Err(error) = std::fs::create_dir_all(&root) {
        trace!("saves: {} cannot be made: {error}", root.display());
        return Err(E_FAIL);
    }
    store::sweep(&root);
    Ok(Box::into_raw(Box::new(Provider { root, quota })))
}

/// Opens the provider of the configuration a title names.
///
/// # Safety
/// `configuration` is null or a null-terminated string.
unsafe fn open(configuration: *const c_char) -> Result<*mut Provider, i32> {
    if configuration.is_null() {
        return Err(E_INVALIDARG);
    }
    // SAFETY: a null-terminated string of the title's.
    let configuration = unsafe { CStr::from_ptr(configuration) }.to_string_lossy();
    let root = title_data()
        .map(|data| root_in(&data, &configuration))
        .ok_or(E_FAIL)?;
    let provider = open_at(root.clone(), QUOTA)?;
    trace!(
        "saves for configuration {configuration} are in {}",
        root.display()
    );
    Ok(provider)
}

/// +0x18 `XGameSaveInitializeProvider`: `(user, configuration id, whether to sync on
/// demand, provider out)`. The blocking form of [`initialize_async`]. There is one user and
/// no service, so the user and the sync flag change nothing.
///
/// # Safety
/// `configuration` is null or a null-terminated string; `out` is null or where the
/// provider goes.
unsafe extern "system" fn initialize(
    _this: usize,
    _user: *const c_void,
    configuration: *const c_char,
    _sync_on_demand: u8,
    out: *mut *mut Provider,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: as received.
    match unsafe { open(configuration) } {
        Ok(provider) => {
            // SAFETY: checked non-null.
            unsafe { out.write(provider) };
            S_OK
        }
        Err(code) => code,
    }
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
    let outcome =
        unsafe { open(configuration) }.map(|provider| (provider as usize).to_ne_bytes().to_vec());
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
        // SAFETY: made by `Box::into_raw` in `open_at`.
        drop(unsafe { Box::from_raw(provider) });
    }
}

/// The quota less the bytes of every blob the provider's containers hold.
fn quota_left(provider: &Provider) -> i64 {
    provider.quota.saturating_sub(store::used(&provider.root)) as i64
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
    *out = quota_left(provider);
    S_OK
}

/// What names a quota call's result.
fn quota_identity() -> usize {
    remaining_quota_async as *const () as usize
}

/// +0x40 `XGameSaveGetRemainingQuotaAsync`: `(provider, block)`.
///
/// # Safety
/// `provider` is null or an open provider; `block` is null or a block that stays valid
/// until the result is taken.
unsafe extern "system" fn remaining_quota_async(
    _this: usize,
    provider: *const Provider,
    block: *mut AsyncBlock,
) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let outcome = unsafe { provider.as_ref() }
        .map(|provider| quota_left(provider).to_ne_bytes().to_vec())
        .ok_or(E_INVALIDARG);
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, quota_identity(), outcome) }
}

/// +0x48 `XGameSaveGetRemainingQuotaResult`: `(block, bytes left out)`.
///
/// # Safety
/// `block` is null or the block of a quota call; `out` is null or where the count goes.
unsafe extern "system" fn remaining_quota_result(
    _this: usize,
    block: *mut AsyncBlock,
    out: *mut i64,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's block, and room for the count.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            quota_identity() as *const c_void,
            size_of::<i64>(),
            out.cast(),
            std::ptr::null_mut(),
        )
    }
}

/// Deletes a container for both forms of the call.
///
/// # Safety
/// `provider` is null or an open provider; `name` is null or a null-terminated string.
unsafe fn delete_now(provider: *const Provider, name: *const c_char) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let Some(provider) = (unsafe { provider.as_ref() }) else {
        return E_INVALIDARG;
    };
    // SAFETY: as received.
    let name = match unsafe { container_name_at(name) } {
        Ok(name) => name,
        Err(code) => return code,
    };
    match store::delete(&provider.root, &name) {
        Ok(()) => {
            trace!("saves: container {name} deleted");
            S_OK
        }
        Err(error) => code(&error, &name),
    }
}

/// +0x50 `XGameSaveDeleteContainer`: `(provider, container name)`. The container and all
/// its blobs go in one step. Deleting a container that does not exist succeeds.
///
/// # Safety
/// `provider` is null or an open provider; `name` is null or a null-terminated string.
unsafe extern "system" fn delete_container(
    _this: usize,
    provider: *const Provider,
    name: *const c_char,
) -> i32 {
    // SAFETY: as received.
    unsafe { delete_now(provider, name) }
}

/// Completes `block` with a result that is a code alone.
///
/// # Safety
/// `block` is null or a block that stays valid until the result is taken.
unsafe fn finish_with_code(block: *mut AsyncBlock, identity: usize, result: i32) -> i32 {
    let outcome = if result < 0 {
        Err(result)
    } else {
        Ok(Vec::new())
    };
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, identity, outcome) }
}

/// Takes the result of a call that [`finish_with_code`] completed.
///
/// # Safety
/// `block` is null or the block of such a call.
unsafe fn code_result(block: *mut AsyncBlock, identity: usize) -> i32 {
    // SAFETY: the title's block; there are no bytes to hand over.
    unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            identity as *const c_void,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }
}

/// What names a delete call's result.
fn delete_identity() -> usize {
    delete_container_async as *const () as usize
}

/// +0x58 `XGameSaveDeleteContainerAsync`: `(provider, container name, block)`.
///
/// # Safety
/// As [`delete_container`]; `block` is null or a block that stays valid until the result
/// is taken.
unsafe extern "system" fn delete_container_async(
    _this: usize,
    provider: *const Provider,
    name: *const c_char,
    block: *mut AsyncBlock,
) -> i32 {
    // SAFETY: as received.
    unsafe { finish_with_code(block, delete_identity(), delete_now(provider, name)) }
}

/// +0x60 `XGameSaveDeleteContainerResult`: `(block)`.
///
/// # Safety
/// `block` is null or the block of a delete call.
unsafe extern "system" fn delete_container_result(_this: usize, block: *mut AsyncBlock) -> i32 {
    // SAFETY: as received.
    unsafe { code_result(block, delete_identity()) }
}

/// Tells the title about each of `found` in turn until its callback answers false. No lock
/// of the store is held here: the callback may call back into this area.
///
/// # Safety
/// `callback` is the title's, to be called with `context`.
unsafe fn tell_containers(found: &[Snapshot], context: *mut c_void, callback: ContainerCallback) {
    for container in found {
        let name = c_text(container.name.as_bytes());
        let display = c_text(&container.display);
        let info = ContainerInfo {
            name: name.as_ptr(),
            display_name: display.as_ptr(),
            blob_count: container.blobs.len() as u32,
            total_size: container.total(),
            last_modified: container.modified,
            needs_sync: false,
        };
        // SAFETY: the title's callback, with information that lives for the call.
        if unsafe { callback(&info, context) } == 0 {
            break;
        }
    }
}

/// +0x68 `XGameSaveGetContainerInfo`: `(provider, container name, context, callback)`. The
/// callback is called once if the container exists. A container that does not exist is no
/// error: the callback is not called.
///
/// # Safety
/// `provider` is null or an open provider; `name` is null or a null-terminated string;
/// `callback` is null or the title's.
unsafe extern "system" fn container_info(
    _this: usize,
    provider: *const Provider,
    name: *const c_char,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let (Some(provider), Some(callback)) = (unsafe { provider.as_ref() }, callback) else {
        return E_INVALIDARG;
    };
    // SAFETY: as received.
    let name = match unsafe { container_name_at(name) } {
        Ok(name) => name,
        Err(code) => return code,
    };
    match store::container(&provider.root, &name) {
        Ok(found) => {
            // SAFETY: the title's callback and context.
            unsafe { tell_containers(found.as_slice(), context, callback) };
            S_OK
        }
        Err(error) => code(&error, &name),
    }
}

/// Lists the provider's containers whose names begin with `prefix`, in the order of their
/// names.
///
/// # Safety
/// `provider` is null or an open provider; `callback` is null or the title's.
unsafe fn list_containers(
    provider: *const Provider,
    prefix: &[u8],
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let (Some(provider), Some(callback)) = (unsafe { provider.as_ref() }, callback) else {
        return E_INVALIDARG;
    };
    let mut found = store::containers(&provider.root);
    found.retain(|container| container.name.as_bytes().starts_with(prefix));
    // SAFETY: the title's callback and context.
    unsafe { tell_containers(&found, context, callback) };
    S_OK
}

/// +0x70 `XGameSaveEnumerateContainerInfo`: `(provider, context, callback)`. The callback
/// is called for every container until it answers false.
///
/// # Safety
/// `provider` is null or an open provider; `callback` is null or the title's.
unsafe extern "system" fn enumerate_containers(
    _this: usize,
    provider: *const Provider,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> i32 {
    // SAFETY: as received.
    unsafe { list_containers(provider, b"", context, callback) }
}

/// +0x78 `XGameSaveEnumerateContainerInfoByName`: `(provider, start of the name, context,
/// callback)`. A start that is null is taken as empty, which lists every container.
///
/// # Safety
/// As [`enumerate_containers`]; `prefix` is null or a null-terminated string.
unsafe extern "system" fn enumerate_containers_by_name(
    _this: usize,
    provider: *const Provider,
    prefix: *const c_char,
    context: *mut c_void,
    callback: Option<ContainerCallback>,
) -> i32 {
    // SAFETY: as received.
    let prefix = unsafe { bytes_at(prefix) }.unwrap_or_default();
    // SAFETY: as received.
    unsafe { list_containers(provider, &prefix, context, callback) }
}

/// +0x80 `XGameSaveCreateContainer`: `(provider, container name, container out)`. Gives a
/// handle to a container that may or may not exist yet. Nothing is made on disk: the
/// container exists once an update to it is submitted.
///
/// # Safety
/// `provider` is null or an open provider; `name` is null or a null-terminated string;
/// `out` is null or where the handle goes.
unsafe extern "system" fn create_container(
    _this: usize,
    provider: *const Provider,
    name: *const c_char,
    out: *mut *mut Container,
) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let Some(provider) = (unsafe { provider.as_ref() }) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: as received.
    let name = match unsafe { container_name_at(name) } {
        Ok(name) => name,
        Err(code) => return code,
    };
    let container = Box::into_raw(Box::new(Container {
        root: provider.root.clone(),
        quota: provider.quota,
        name,
    }));
    // SAFETY: checked non-null.
    unsafe { out.write(container) };
    S_OK
}

/// +0x88 `XGameSaveCloseContainer`.
///
/// # Safety
/// `container` is null or one from [`create_container`], closed once.
unsafe extern "system" fn close_container(_this: usize, container: *mut Container) {
    if !container.is_null() {
        // SAFETY: made by `Box::into_raw` in `create_container`.
        drop(unsafe { Box::from_raw(container) });
    }
}

/// Lists the container's blobs whose names begin with `prefix`, in the order of their
/// names. A container that does not exist yet has none.
///
/// # Safety
/// `container` is null or an open container; `callback` is null or the title's.
unsafe fn list_blobs(
    container: *const Container,
    prefix: &[u8],
    context: *mut c_void,
    callback: Option<BlobCallback>,
) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let (Some(container), Some(callback)) = (unsafe { container.as_ref() }, callback) else {
        return E_INVALIDARG;
    };
    let blobs = match store::container(&container.root, &container.name) {
        Ok(found) => found.map(|found| found.blobs).unwrap_or_default(),
        Err(error) => return code(&error, &container.name),
    };
    // The store's lock is no longer held: the callback may call back into this area.
    for blob in blobs.iter().filter(|blob| blob.name.starts_with(prefix)) {
        let name = c_text(&blob.name);
        let info = BlobInfo {
            name: name.as_ptr(),
            size: blob.size as u32,
        };
        // SAFETY: the title's callback, with information that lives for the call.
        if unsafe { callback(&info, context) } == 0 {
            break;
        }
    }
    S_OK
}

/// +0x90 `XGameSaveEnumerateBlobInfo`: `(container, context, callback)`. The callback is
/// called for every blob until it answers false.
///
/// # Safety
/// `container` is null or an open container; `callback` is null or the title's.
unsafe extern "system" fn enumerate_blobs(
    _this: usize,
    container: *const Container,
    context: *mut c_void,
    callback: Option<BlobCallback>,
) -> i32 {
    // SAFETY: as received.
    unsafe { list_blobs(container, b"", context, callback) }
}

/// +0x98 `XGameSaveEnumerateBlobInfoByName`: `(container, start of the name, context,
/// callback)`. A start that is null is taken as empty, which lists every blob.
///
/// # Safety
/// As [`enumerate_blobs`]; `prefix` is null or a null-terminated string.
unsafe extern "system" fn enumerate_blobs_by_name(
    _this: usize,
    container: *const Container,
    prefix: *const c_char,
    context: *mut c_void,
    callback: Option<BlobCallback>,
) -> i32 {
    // SAFETY: as received.
    let prefix = unsafe { bytes_at(prefix) }.unwrap_or_default();
    // SAFETY: as received.
    unsafe { list_blobs(container, &prefix, context, callback) }
}

/// Reads blobs for both forms of the call: the `count` named ones, or all of them if
/// `names` is null. A name the container does not have is "blob not found"; so is, by
/// assumption, any read from a container that does not exist.
///
/// # Safety
/// `container` is null or an open container; `names` is null or `count` pointers, each
/// null or a null-terminated string.
unsafe fn read_now(
    container: *const Container,
    names: *const *const c_char,
    count: u32,
) -> Result<Vec<NamedBlob>, i32> {
    // SAFETY: the title's handle; null is refused.
    let container = unsafe { container.as_ref() }.ok_or(E_INVALIDARG)?;
    let wanted = if names.is_null() {
        None
    } else {
        // SAFETY: the title passes `count` pointers.
        let names = unsafe { std::slice::from_raw_parts(names, count as usize) };
        let wanted: Option<Vec<Vec<u8>>> = names
            .iter()
            // SAFETY: each is null or a null-terminated string.
            .map(|&name| unsafe { bytes_at(name) })
            .collect();
        Some(wanted.ok_or(E_INVALIDARG)?)
    };
    store::read(&container.root, &container.name, wanted.as_deref())
        .map_err(|error| code(&error, &container.name))
}

/// Blobs as a read hands them over, for a buffer at address zero: the array of [`Blob`],
/// then each blob's name with its terminator and its bytes. Each pointer is the place in
/// these bytes, to which [`rebase`] adds the address of the buffer they are copied to.
fn lay_out(blobs: &[NamedBlob]) -> Vec<u8> {
    const NAME_AT: usize = std::mem::offset_of!(Blob, info) + std::mem::offset_of!(BlobInfo, name);
    const SIZE_AT: usize = std::mem::offset_of!(Blob, info) + std::mem::offset_of!(BlobInfo, size);
    const DATA_AT: usize = std::mem::offset_of!(Blob, data);
    let mut out = vec![0u8; blobs.len() * size_of::<Blob>()];
    for (index, (name, bytes)) in blobs.iter().enumerate() {
        let name_at = out.len();
        out.extend_from_slice(name);
        out.push(0);
        let data_at = out.len();
        out.extend_from_slice(bytes);
        let entry = &mut out[index * size_of::<Blob>()..][..size_of::<Blob>()];
        entry[NAME_AT..NAME_AT + 8].copy_from_slice(&name_at.to_ne_bytes());
        entry[SIZE_AT..SIZE_AT + 4].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
        entry[DATA_AT..DATA_AT + 8].copy_from_slice(&data_at.to_ne_bytes());
    }
    out
}

/// Turns the places in `count` laid-out blobs at `buffer` into pointers into `buffer`.
///
/// # Safety
/// `buffer` holds what [`lay_out`] made for `count` blobs.
unsafe fn rebase(buffer: *mut u8, count: usize) {
    for index in 0..count {
        for field in [0, std::mem::offset_of!(Blob, data)] {
            // SAFETY: a pointer-sized field of the entry at `index`, inside the buffer. The
            // title's buffer need not be aligned.
            unsafe {
                let place = buffer
                    .add(index * size_of::<Blob>() + field)
                    .cast::<usize>();
                place.write_unaligned(place.read_unaligned() + buffer as usize);
            }
        }
    }
}

/// +0xa0 `XGameSaveReadBlobData`: `(container, blob names or null for all, count in and
/// out, buffer size, buffer)`. The buffer is filled with an array of `XGameSaveBlob`
/// followed by the names and bytes it points to, and the count becomes the number of blobs
/// read. A buffer that is too small is refused with "buffer too small" and nothing is
/// written.
///
/// # Safety
/// `container` is null or an open container; `names` is null or `*count` pointers to
/// null-terminated strings; `count` is null or a count; `buffer` is null or `size`
/// writable bytes.
unsafe extern "system" fn read_blobs(
    _this: usize,
    container: *const Container,
    names: *const *const c_char,
    count: *mut u32,
    size: usize,
    buffer: *mut u8,
) -> i32 {
    // SAFETY: the title's count; null is refused.
    let Some(count) = (unsafe { count.as_mut() }) else {
        return E_INVALIDARG;
    };
    // SAFETY: as received.
    let blobs = match unsafe { read_now(container, names, *count) } {
        Ok(blobs) => blobs,
        Err(code) => return code,
    };
    let bytes = lay_out(&blobs);
    if size < bytes.len() || (buffer.is_null() && !bytes.is_empty()) {
        return BUFFER_TOO_SMALL;
    }
    if !bytes.is_empty() {
        // SAFETY: the buffer has room for the bytes, as checked, and they are what
        // `lay_out` made for these blobs.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
            rebase(buffer, blobs.len());
        }
    }
    *count = blobs.len() as u32;
    S_OK
}

/// What names a read call's result.
fn read_identity() -> usize {
    read_blobs_async as *const () as usize
}

/// +0xa8 `XGameSaveReadBlobDataAsync`: `(container, blob names or null for all, count,
/// block)`. The blobs are read at once; the size of the call's result is the buffer the
/// title needs for [`read_blobs_result`].
///
/// # Safety
/// As [`read_blobs`]; `block` is null or a block that stays valid until the result is
/// taken.
unsafe extern "system" fn read_blobs_async(
    _this: usize,
    container: *const Container,
    names: *const *const c_char,
    count: u32,
    block: *mut AsyncBlock,
) -> i32 {
    // SAFETY: as received.
    let outcome = unsafe { read_now(container, names, count) }.map(|blobs| lay_out(&blobs));
    // SAFETY: the title's block.
    unsafe { xasync::finish(block, read_identity(), outcome) }
}

/// +0xb0 `XGameSaveReadBlobDataResult`: `(block, buffer size, buffer, count out)`. Fills
/// the buffer as [`read_blobs`] does. A buffer that is too small is refused with "buffer
/// too small", and the result stays to be taken with a larger one.
///
/// # Safety
/// `block` is null or the block of a read call; `buffer` is null or `size` writable bytes;
/// `count` is null or where the count goes.
unsafe extern "system" fn read_blobs_result(
    _this: usize,
    block: *mut AsyncBlock,
    size: usize,
    buffer: *mut u8,
    count: *mut u32,
) -> i32 {
    let mut used = 0;
    // SAFETY: the title's block and buffer.
    let result = unsafe {
        xasync::get_result(
            std::ptr::null_mut(),
            block,
            read_identity() as *const c_void,
            size,
            buffer.cast(),
            &mut used,
        )
    };
    if result == E_NOT_SUFFICIENT_BUFFER {
        return BUFFER_TOO_SMALL;
    }
    if result < 0 {
        return result;
    }
    // The first blob's name lies right after the array, so its place tells the array's
    // length. A result of no bytes is no blobs.
    let blobs = if used == 0 {
        0
    } else {
        // SAFETY: a result that is not empty begins with one blob, whose first field is
        // the place of its name.
        unsafe { buffer.cast::<usize>().read_unaligned() / size_of::<Blob>() }
    };
    // SAFETY: the buffer holds what `lay_out` made for that many blobs.
    unsafe { rebase(buffer, blobs) };
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(count) = unsafe { count.as_mut() } {
        *count = blobs as u32;
    }
    S_OK
}

/// +0xb8 `XGameSaveCreateUpdate`: `(container, display name, update out)`. Begins an
/// update, which changes nothing until it is submitted. A display name that is null keeps
/// the one the container has.
///
/// # Safety
/// `container` is null or an open container; `display` is null or a null-terminated
/// string; `out` is null or where the handle goes.
unsafe extern "system" fn create_update(
    _this: usize,
    container: *const Container,
    display: *const c_char,
    out: *mut *mut Update,
) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let Some(container) = (unsafe { container.as_ref() }) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let update = Box::into_raw(Box::new(Update {
        root: container.root.clone(),
        quota: container.quota,
        name: container.name.clone(),
        // SAFETY: as received.
        display: unsafe { bytes_at(display) },
        changes: Mutex::new(Vec::new()),
    }));
    // SAFETY: checked non-null.
    unsafe { out.write(update) };
    S_OK
}

/// +0xc0 `XGameSaveCloseUpdate`. An update that was never submitted is dropped with all it
/// gathered.
///
/// # Safety
/// `update` is null or one from [`create_update`], closed once.
unsafe extern "system" fn close_update(_this: usize, update: *mut Update) {
    if !update.is_null() {
        // SAFETY: made by `Box::into_raw` in `create_update`.
        drop(unsafe { Box::from_raw(update) });
    }
}

/// +0xc8 `XGameSaveSubmitBlobWrite`: `(update, blob name, data, size)`. Adds a blob to
/// write to the update; the bytes are copied now, so the title may free its own. One blob
/// larger than a whole update may be is refused here; the sum of an update's writes is
/// checked when it is submitted.
///
/// # Safety
/// `update` is null or an open update; `name` is null or a null-terminated string; `data`
/// is null or `size` readable bytes.
unsafe extern "system" fn submit_blob_write(
    _this: usize,
    update: *const Update,
    name: *const c_char,
    data: *const u8,
    size: usize,
) -> i32 {
    // SAFETY: the title's handle and string; null is refused.
    let (Some(update), Some(name)) = (unsafe { (update.as_ref(), bytes_at(name)) }) else {
        return E_INVALIDARG;
    };
    if name.is_empty() || (data.is_null() && size > 0) {
        return E_INVALIDARG;
    }
    if size as u64 > store::UPDATE_LIMIT {
        return UPDATE_TOO_BIG;
    }
    let bytes = if size == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `size` bytes.
        unsafe { std::slice::from_raw_parts(data, size) }.to_vec()
    };
    update.changes().push(Change::Write(name, bytes));
    S_OK
}

/// +0xd0 `XGameSaveSubmitBlobDelete`: `(update, blob name)`. Adds a blob to delete to the
/// update. Deleting a blob the container does not have is assumed to be no error.
///
/// # Safety
/// `update` is null or an open update; `name` is null or a null-terminated string.
unsafe extern "system" fn submit_blob_delete(
    _this: usize,
    update: *const Update,
    name: *const c_char,
) -> i32 {
    // SAFETY: the title's handle and string; null is refused.
    let (Some(update), Some(name)) = (unsafe { (update.as_ref(), bytes_at(name)) }) else {
        return E_INVALIDARG;
    };
    if name.is_empty() {
        return E_INVALIDARG;
    }
    update.changes().push(Change::Delete(name));
    S_OK
}

/// Applies an update for both forms of the call.
///
/// # Safety
/// `update` is null or an open update.
unsafe fn submit_now(update: *const Update) -> i32 {
    // SAFETY: the title's handle; null is refused.
    let Some(update) = (unsafe { update.as_ref() }) else {
        return E_INVALIDARG;
    };
    let changes = update.changes();
    let applied = store::apply(
        &update.root,
        &update.name,
        update.display.as_deref(),
        &changes,
        update.quota,
    );
    match applied {
        Ok(()) => {
            trace!(
                "saves: container {} updated with {} changes",
                update.name,
                changes.len()
            );
            S_OK
        }
        Err(error) => code(&error, &update.name),
    }
}

/// +0xd8 `XGameSaveSubmitUpdate`: `(update)`. Applies the update's writes and deletions,
/// in the order they were added, and its display name as one step: the container is made
/// if it does not exist, and after a crash at any moment it is either what it was or what
/// the update makes it. When this returns success the save is on the drive.
///
/// # Safety
/// `update` is null or an open update.
unsafe extern "system" fn submit_update(_this: usize, update: *const Update) -> i32 {
    // SAFETY: as received.
    unsafe { submit_now(update) }
}

/// What names a submit call's result.
fn submit_identity() -> usize {
    submit_update_async as *const () as usize
}

/// +0xe0 `XGameSaveSubmitUpdateAsync`: `(update, block)`. The update is applied before
/// this returns, and the block completes with how it went.
///
/// # Safety
/// `update` is null or an open update; `block` is null or a block that stays valid until
/// the result is taken.
unsafe extern "system" fn submit_update_async(
    _this: usize,
    update: *const Update,
    block: *mut AsyncBlock,
) -> i32 {
    // SAFETY: as received.
    unsafe { finish_with_code(block, submit_identity(), submit_now(update)) }
}

/// +0xe8 `XGameSaveSubmitUpdateResult`: `(block)`.
///
/// # Safety
/// `block` is null or the block of a submit call.
unsafe extern "system" fn submit_update_result(_this: usize, block: *mut AsyncBlock) -> i32 {
    // SAFETY: as received.
    unsafe { code_result(block, submit_identity()) }
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (3, initialize as *const () as usize),
        (4, initialize_async as *const () as usize),
        (5, initialize_result as *const () as usize),
        (6, close_provider as *const () as usize),
        (7, remaining_quota as *const () as usize),
        (8, remaining_quota_async as *const () as usize),
        (9, remaining_quota_result as *const () as usize),
        (10, delete_container as *const () as usize),
        (11, delete_container_async as *const () as usize),
        (12, delete_container_result as *const () as usize),
        (13, container_info as *const () as usize),
        (14, enumerate_containers as *const () as usize),
        (15, enumerate_containers_by_name as *const () as usize),
        (16, create_container as *const () as usize),
        (17, close_container as *const () as usize),
        (18, enumerate_blobs as *const () as usize),
        (19, enumerate_blobs_by_name as *const () as usize),
        (20, read_blobs as *const () as usize),
        (21, read_blobs_async as *const () as usize),
        (22, read_blobs_result as *const () as usize),
        (23, create_update as *const () as usize),
        (24, close_update as *const () as usize),
        (25, submit_blob_write as *const () as usize),
        (26, submit_blob_delete as *const () as usize),
        (27, submit_update as *const () as usize),
        (28, submit_update_async as *const () as usize),
        (29, submit_update_result as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A fresh folder for one test, removed when the test is over.
    struct Folder(PathBuf);

    impl Folder {
        fn new(test: &str) -> Folder {
            let path = std::env::temp_dir()
                .join(format!("riptide-gamesave-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Folder(path)
        }

        /// Opens a provider on this folder, as a run of the title does.
        fn open(&self, quota: u64) -> *mut Provider {
            open_at(self.0.join("saves"), quota).unwrap()
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn c(text: &str) -> CString {
        CString::new(text).unwrap()
    }

    /// A block with no completion routine: zeroes are a valid start.
    struct Block([AtomicUsize; 7]);

    impl Block {
        fn new() -> Block {
            Block([const { AtomicUsize::new(0) }; 7])
        }

        fn get(&mut self) -> *mut AsyncBlock {
            self.0.as_mut_ptr().cast()
        }
    }

    /// What a title's callbacks gathered.
    struct Seen {
        containers: Vec<(String, String, u32, u64)>,
        blobs: Vec<(String, u32)>,
        /// The callbacks answer false once this many have been seen.
        limit: usize,
        /// If set, the container callback calls back into the area with it.
        provider: *const Provider,
    }

    impl Seen {
        fn new(limit: usize) -> Seen {
            Seen {
                containers: Vec::new(),
                blobs: Vec::new(),
                limit,
                provider: std::ptr::null(),
            }
        }

        fn context(&mut self) -> *mut c_void {
            (self as *mut Seen).cast()
        }
    }

    unsafe extern "system" fn see_container(
        info: *const ContainerInfo,
        context: *mut c_void,
    ) -> u8 {
        // SAFETY: every test passes a `Seen` as the context, and the area passes live
        // information.
        let (seen, info) = unsafe { (&mut *context.cast::<Seen>(), &*info) };
        assert!(!info.needs_sync);
        assert!(info.last_modified > 1_700_000_000);
        if !seen.provider.is_null() {
            // A title may ask the area something from inside its callback.
            let mut left = 0;
            // SAFETY: a live provider and a valid out-pointer.
            let asked = unsafe { remaining_quota(0, seen.provider, &mut left) };
            assert_eq!(asked, S_OK);
        }
        // SAFETY: null-terminated strings that live for the call.
        let (name, display) = unsafe {
            (
                CStr::from_ptr(info.name).to_str().unwrap().to_owned(),
                CStr::from_ptr(info.display_name)
                    .to_str()
                    .unwrap()
                    .to_owned(),
            )
        };
        seen.containers
            .push((name, display, info.blob_count, info.total_size));
        u8::from(seen.containers.len() < seen.limit)
    }

    unsafe extern "system" fn see_blob(info: *const BlobInfo, context: *mut c_void) -> u8 {
        // SAFETY: as in `see_container`.
        let (seen, info) = unsafe { (&mut *context.cast::<Seen>(), &*info) };
        // SAFETY: a null-terminated string that lives for the call.
        let name = unsafe { CStr::from_ptr(info.name) }
            .to_str()
            .unwrap()
            .to_owned();
        seen.blobs.push((name, info.size));
        u8::from(seen.blobs.len() < seen.limit)
    }

    /// A handle to the container `name`.
    fn container(provider: *const Provider, name: &str) -> *mut Container {
        let mut out = std::ptr::null_mut();
        let name = c(name);
        // SAFETY: a live provider, a string and a valid out-pointer.
        let made = unsafe { create_container(0, provider, name.as_ptr(), &mut out) };
        assert_eq!(made, S_OK);
        out
    }

    /// Submits one update through the blocking form or the asynchronous one.
    fn update(
        container: *const Container,
        display: Option<&str>,
        writes: &[(&str, &[u8])],
        deletes: &[&str],
        asynchronous: bool,
    ) -> i32 {
        let display = display.map(c);
        let display = display.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        let mut handle = std::ptr::null_mut();
        // SAFETY: a live container, strings, and buffers of the sizes given.
        unsafe {
            assert_eq!(create_update(0, container, display, &mut handle), S_OK);
            for (name, bytes) in writes {
                let name = c(name);
                let wrote =
                    submit_blob_write(0, handle, name.as_ptr(), bytes.as_ptr(), bytes.len());
                assert_eq!(wrote, S_OK);
            }
            for name in deletes {
                assert_eq!(submit_blob_delete(0, handle, c(name).as_ptr()), S_OK);
            }
            let result = if asynchronous {
                let mut block = Block::new();
                assert_eq!(submit_update_async(0, handle, block.get()), S_OK);
                submit_update_result(0, block.get())
            } else {
                submit_update(0, handle)
            };
            close_update(0, handle);
            result
        }
    }

    fn containers(
        provider: *const Provider,
        prefix: Option<&str>,
    ) -> Vec<(String, String, u32, u64)> {
        let mut seen = Seen::new(usize::MAX);
        // SAFETY: a live provider, and a callback that takes the context given.
        let result = unsafe {
            match prefix {
                None => enumerate_containers(0, provider, seen.context(), Some(see_container)),
                Some(prefix) => enumerate_containers_by_name(
                    0,
                    provider,
                    c(prefix).as_ptr(),
                    seen.context(),
                    Some(see_container),
                ),
            }
        };
        assert_eq!(result, S_OK);
        seen.containers
    }

    fn blob_list(container: *const Container, prefix: Option<&str>) -> Vec<(String, u32)> {
        let mut seen = Seen::new(usize::MAX);
        // SAFETY: a live container, and a callback that takes the context given.
        let result = unsafe {
            match prefix {
                None => enumerate_blobs(0, container, seen.context(), Some(see_blob)),
                Some(prefix) => enumerate_blobs_by_name(
                    0,
                    container,
                    c(prefix).as_ptr(),
                    seen.context(),
                    Some(see_blob),
                ),
            }
        };
        assert_eq!(result, S_OK);
        seen.blobs
    }

    /// Reads blobs into a buffer of `room` bytes through either form, and gives what the
    /// buffer then holds as a title would read it.
    fn read(
        container: *const Container,
        names: Option<&[&str]>,
        room: usize,
        asynchronous: bool,
    ) -> Result<Vec<(String, Vec<u8>)>, i32> {
        let owned: Vec<CString> = names.unwrap_or_default().iter().map(|n| c(n)).collect();
        let pointers: Vec<*const c_char> = owned.iter().map(|name| name.as_ptr()).collect();
        let names = if names.is_some() {
            pointers.as_ptr()
        } else {
            std::ptr::null()
        };
        // Words, so that the array of blobs is aligned as a title's would be.
        let mut buffer = vec![0u64; room.div_ceil(8)];
        let start = buffer.as_mut_ptr().cast::<u8>();
        let mut count = pointers.len() as u32;
        // SAFETY: a live container, `count` names, and a buffer of at least `room` bytes.
        let result = unsafe {
            if asynchronous {
                let mut block = Block::new();
                assert_eq!(
                    read_blobs_async(0, container, names, count, block.get()),
                    S_OK
                );
                // Too little room first, when there is anything to hand over: the result
                // stays for the second try.
                let mut needed = 0;
                let sized = xasync::get_result_size(std::ptr::null_mut(), block.get(), &mut needed);
                if sized >= 0 && needed > 0 {
                    assert_eq!(
                        read_blobs_result(0, block.get(), needed - 1, start, &mut count),
                        BUFFER_TOO_SMALL
                    );
                }
                read_blobs_result(0, block.get(), room, start, &mut count)
            } else {
                read_blobs(0, container, names, &mut count, room, start)
            }
        };
        if result < 0 {
            if !asynchronous {
                assert_eq!(
                    count,
                    pointers.len() as u32,
                    "a failed read leaves the count"
                );
            }
            return Err(result);
        }
        let inside = start as usize..start as usize + room;
        let blobs = (0..count as usize).map(|index| {
            // SAFETY: the area wrote `count` blobs whose pointers lie in the buffer, which
            // is checked before they are followed.
            unsafe {
                let blob = &*start.cast::<Blob>().add(index);
                assert!(inside.contains(&(blob.info.name as usize)));
                assert!(inside.contains(&(blob.data as usize)) || blob.info.size == 0);
                let name = CStr::from_ptr(blob.info.name).to_str().unwrap().to_owned();
                let bytes = std::slice::from_raw_parts(blob.data, blob.info.size as usize);
                (name, bytes.to_vec())
            }
        });
        Ok(blobs.collect())
    }

    fn named(blobs: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
        blobs
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
            .collect()
    }

    fn quota(provider: *const Provider) -> i64 {
        let mut left = 0;
        // SAFETY: a live provider and a valid out-pointer.
        assert_eq!(unsafe { remaining_quota(0, provider, &mut left) }, S_OK);
        left
    }

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
        let folder = Folder::new("quota-left");
        let provider = folder.open(QUOTA);
        assert_eq!(quota(provider), QUOTA as i64);
        let slot = container(provider, "container");
        assert_eq!(
            update(slot, None, &[("blob", &[0; 1000])], &[], false),
            S_OK
        );
        assert_eq!(quota(provider), (QUOTA - 1000) as i64);
        let mut left = 0;
        // SAFETY: the null handle and the null out-pointer under test, then live handles.
        unsafe {
            assert_eq!(
                remaining_quota(0, std::ptr::null(), &mut left),
                E_INVALIDARG
            );
            assert_eq!(
                remaining_quota(0, provider, std::ptr::null_mut()),
                E_INVALIDARG
            );
            close_container(0, slot);
            close_provider(0, provider);
        }
    }

    #[test]
    fn an_update_is_written_and_read_back_and_is_there_on_the_next_run() {
        let folder = Folder::new("round-trip");
        let world = [7u8; 300];
        let written: [(&str, &[u8]); 2] = [("world", &world), ("meta", b"header")];
        let in_order: [(&str, &[u8]); 2] = [("meta", b"header"), ("world", &world)];
        // The array of two blobs, the two names with their terminators, and the bytes.
        let needed = 2 * 24 + 5 + 6 + 6 + 300;

        let provider = folder.open(QUOTA);
        let slot = container(provider, "slots/slot_1");
        let name = c("slots/slot_1");
        let mut seen = Seen::new(usize::MAX);
        // SAFETY: live handles, and a callback that takes the context given.
        unsafe {
            // A handle alone makes no container: nothing is told, and that is no error.
            let told = container_info(
                0,
                provider,
                name.as_ptr(),
                seen.context(),
                Some(see_container),
            );
            assert_eq!(told, S_OK);
        }
        assert!(seen.containers.is_empty());
        assert!(containers(provider, None).is_empty());
        assert!(blob_list(slot, None).is_empty());
        assert_eq!(read(slot, None, 4096, false), Err(BLOB_NOT_FOUND));

        assert_eq!(update(slot, Some("Slot one"), &written, &[], false), S_OK);

        let expected = [("slots/slot_1".to_owned(), "Slot one".to_owned(), 2, 306)];
        // SAFETY: as above.
        unsafe {
            let told = container_info(
                0,
                provider,
                name.as_ptr(),
                seen.context(),
                Some(see_container),
            );
            assert_eq!(told, S_OK);
        }
        assert_eq!(seen.containers, expected);
        assert_eq!(containers(provider, None), expected);
        assert_eq!(
            blob_list(slot, None),
            [("meta".to_owned(), 6), ("world".to_owned(), 300)]
        );
        assert_eq!(blob_list(slot, Some("wo")), [("world".to_owned(), 300)]);
        assert!(blob_list(slot, Some("x")).is_empty());

        assert_eq!(read(slot, None, needed, false), Ok(named(&in_order)));
        assert_eq!(
            read(slot, Some(&["world", "meta"]), needed, false),
            Ok(named(&written))
        );
        assert_eq!(
            read(slot, Some(&["meta"]), 64, false),
            Ok(named(&in_order[..1]))
        );
        assert_eq!(read(slot, Some(&[]), 0, false), Ok(Vec::new()));
        assert_eq!(read(slot, None, needed - 1, false), Err(BUFFER_TOO_SMALL));
        assert_eq!(read(slot, None, 0, false), Err(BUFFER_TOO_SMALL));
        assert_eq!(
            read(slot, Some(&["meta", "none"]), 4096, false),
            Err(BLOB_NOT_FOUND)
        );
        assert_eq!(
            read(slot, Some(&["META"]), 4096, false),
            Err(BLOB_NOT_FOUND)
        );

        // SAFETY: handles closed once.
        unsafe {
            close_container(0, slot);
            close_provider(0, provider);
        }

        // The next run of the title finds the save whole.
        let provider = folder.open(QUOTA);
        let slot = container(provider, "slots/slot_1");
        assert_eq!(containers(provider, None), expected);
        assert_eq!(read(slot, None, needed, false), Ok(named(&in_order)));
        // SAFETY: handles closed once.
        unsafe {
            close_container(0, slot);
            close_provider(0, provider);
        }
    }

    #[test]
    fn a_second_update_replaces_one_blob_deletes_another_and_keeps_a_third() {
        let folder = Folder::new("second");
        let provider = folder.open(QUOTA);
        let slot = container(provider, "slot");
        let first: [(&str, &[u8]); 3] = [("keep", b"kept"), ("replace", b"old"), ("remove", b"x")];
        assert_eq!(update(slot, Some("First"), &first, &[], false), S_OK);

        // No display name, a write, a deletion, and a deletion of a blob that is not there.
        let second: [(&str, &[u8]); 1] = [("replace", b"new, and longer")];
        assert_eq!(
            update(slot, None, &second, &["remove", "never there"], false),
            S_OK
        );
        let after: [(&str, &[u8]); 2] = [("keep", b"kept"), ("replace", b"new, and longer")];
        assert_eq!(read(slot, None, 4096, false), Ok(named(&after)));
        assert_eq!(
            containers(provider, None),
            [("slot".to_owned(), "First".to_owned(), 2, 19)]
        );
        assert_eq!(quota(provider), QUOTA as i64 - 19);

        // An update with a display name renames and keeps every blob it does not name. A
        // blob may be empty.
        assert_eq!(
            update(slot, Some("Renamed"), &[("empty", b"")], &[], false),
            S_OK
        );
        assert_eq!(
            containers(provider, None),
            [("slot".to_owned(), "Renamed".to_owned(), 3, 19)]
        );
        let last: [(&str, &[u8]); 3] = [
            ("empty", b""),
            ("keep", b"kept"),
            ("replace", b"new, and longer"),
        ];
        assert_eq!(read(slot, None, 4096, false), Ok(named(&last)));
        // SAFETY: handles closed once.
        unsafe {
            close_container(0, slot);
            close_provider(0, provider);
        }
    }

    #[test]
    fn a_deleted_container_is_gone_and_listings_stop_when_told_to() {
        let folder = Folder::new("delete");
        let provider = folder.open(QUOTA);
        for name in ["auto/1", "auto/2", "manual"] {
            let slot = container(provider, name);
            assert_eq!(
                update(slot, Some(name), &[("blob", b"12345")], &[], false),
                S_OK
            );
            // SAFETY: closed once.
            unsafe { close_container(0, slot) };
        }
        let names = |prefix| -> Vec<String> {
            let found = containers(provider, prefix);
            found.into_iter().map(|found| found.0).collect()
        };
        assert_eq!(names(None), ["auto/1", "auto/2", "manual"]);
        assert_eq!(names(Some("auto/")), ["auto/1", "auto/2"]);
        assert_eq!(names(Some("")), ["auto/1", "auto/2", "manual"]);
        assert!(names(Some("Auto")).is_empty());

        // A callback that answers false is not called again, and one that calls back into
        // the area is answered: no lock is held while it runs.
        let mut seen = Seen::new(2);
        seen.provider = provider;
        // SAFETY: live handles, and callbacks that take the context given.
        unsafe {
            assert_eq!(
                enumerate_containers(0, provider, seen.context(), Some(see_container)),
                S_OK
            );
            assert_eq!(seen.containers.len(), 2);
            let slot = container(provider, "manual");
            let mut one = Seen::new(1);
            assert_eq!(update(slot, None, &[("more", b"1")], &[], false), S_OK);
            assert_eq!(
                enumerate_blobs(0, slot, one.context(), Some(see_blob)),
                S_OK
            );
            assert_eq!(one.blobs, [("blob".to_owned(), 5)]);
            assert_eq!(enumerate_blobs(0, slot, one.context(), None), E_INVALIDARG);
            close_container(0, slot);
        }

        let gone = container(provider, "auto/1");
        // SAFETY: a live provider and strings.
        unsafe {
            assert_eq!(delete_container(0, provider, c("auto/1").as_ptr()), S_OK);
            // Deleting what is not there succeeds, by either form.
            assert_eq!(delete_container(0, provider, c("auto/1").as_ptr()), S_OK);
            let mut block = Block::new();
            let begun = delete_container_async(0, provider, c("never").as_ptr(), block.get());
            assert_eq!(begun, S_OK);
            assert_eq!(delete_container_result(0, block.get()), S_OK);
            assert_eq!(
                delete_container(0, std::ptr::null(), c("auto/2").as_ptr()),
                E_INVALIDARG
            );
        }
        assert_eq!(names(None), ["auto/2", "manual"]);
        assert_eq!(read(gone, None, 4096, false), Err(BLOB_NOT_FOUND));
        assert!(blob_list(gone, None).is_empty());
        assert_eq!(quota(provider), QUOTA as i64 - 11);
        // The handle still works: an update makes the container anew, with only the new blob.
        assert_eq!(update(gone, None, &[("fresh", b"1")], &[], false), S_OK);
        assert_eq!(read(gone, None, 4096, false), Ok(named(&[("fresh", b"1")])));
        // SAFETY: handles closed once.
        unsafe {
            close_container(0, gone);
            close_provider(0, provider);
        }
    }

    #[test]
    fn names_that_break_the_rules_are_refused_and_none_leaves_the_folder() {
        for name in ["slot", "a/b/c.sav", "A-b_c.1", "dir_1/.hidden", "/lead"] {
            assert!(valid_container_name(name), "{name}");
        }
        let long = "a".repeat(257);
        for name in [
            "",
            "..",
            "../escape",
            r"..\escape",
            r"C:\escape",
            "a/..",
            "a..b",
            "name.",
            "dir.x/file",
            "dir/",
            "a b",
            "caf\u{e9}",
            &long,
        ] {
            assert!(!valid_container_name(name), "{name}");
        }
        assert!(valid_container_name(&long[1..]));

        let folder = Folder::new("names");
        let provider = folder.open(QUOTA);
        let mut seen = Seen::new(usize::MAX);
        let mut out = std::ptr::null_mut();
        // SAFETY: a live provider, strings, and valid pointers; the null ones are under test.
        unsafe {
            for name in ["../escape", r"..\..\escape", "name.", ""] {
                let name = c(name);
                assert_eq!(
                    create_container(0, provider, name.as_ptr(), &mut out),
                    INVALID_CONTAINER_NAME
                );
                assert_eq!(
                    container_info(
                        0,
                        provider,
                        name.as_ptr(),
                        seen.context(),
                        Some(see_container)
                    ),
                    INVALID_CONTAINER_NAME
                );
                assert_eq!(
                    delete_container(0, provider, name.as_ptr()),
                    INVALID_CONTAINER_NAME
                );
            }
            assert!(out.is_null());
            assert_eq!(
                create_container(0, provider, std::ptr::null(), &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                create_container(0, provider, c("slot").as_ptr(), std::ptr::null_mut()),
                E_INVALIDARG
            );
        }

        // Names that a file system would take for one another are different containers,
        // and so are blob names, whatever they hold.
        let (upper, lower, dotted) = (
            container(provider, "Slot"),
            container(provider, "slot"),
            container(provider, "slot.1"),
        );
        let blobs: [(&str, &[u8]); 4] = [
            ("Data", b"upper"),
            ("data", b"lower"),
            (r"..\..\data", b"climbing"),
            ("con:?*", b"odd"),
        ];
        assert_eq!(update(upper, Some("Upper"), &blobs, &[], false), S_OK);
        assert_eq!(
            update(lower, Some("Lower"), &[("data", b"other")], &[], false),
            S_OK
        );
        assert_eq!(
            update(dotted, None, &[("data", b"third")], &[], false),
            S_OK
        );
        assert_eq!(
            read(
                upper,
                Some(&["data", "Data", r"..\..\data", "con:?*"]),
                4096,
                false
            ),
            Ok(named(&[
                ("data", b"lower"),
                ("Data", b"upper"),
                (r"..\..\data", b"climbing"),
                ("con:?*", b"odd")
            ]))
        );
        assert_eq!(
            read(lower, None, 4096, false),
            Ok(named(&[("data", b"other")]))
        );
        assert_eq!(
            containers(provider, None),
            [
                ("Slot".to_owned(), "Upper".to_owned(), 4, 21),
                ("slot".to_owned(), "Lower".to_owned(), 1, 5),
                ("slot.1".to_owned(), String::new(), 1, 5)
            ]
        );
        // Everything written lies in the provider's folder, which is all the test's folder
        // holds.
        let top: Vec<_> = std::fs::read_dir(&folder.0).unwrap().flatten().collect();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].file_name(), "saves");
        assert_eq!(
            std::fs::read_dir(folder.0.join("saves")).unwrap().count(),
            3
        );

        // SAFETY: handles closed once; an empty blob name and null arguments are refused.
        unsafe {
            let mut handle = std::ptr::null_mut();
            assert_eq!(create_update(0, upper, std::ptr::null(), &mut handle), S_OK);
            assert_eq!(
                submit_blob_write(0, handle, c("").as_ptr(), b"x".as_ptr(), 1),
                E_INVALIDARG
            );
            assert_eq!(
                submit_blob_write(0, handle, c("x").as_ptr(), std::ptr::null(), 1),
                E_INVALIDARG
            );
            assert_eq!(
                submit_blob_delete(0, handle, std::ptr::null()),
                E_INVALIDARG
            );
            assert_eq!(submit_update(0, std::ptr::null()), E_INVALIDARG);
            close_update(0, handle);
            for handle in [upper, lower, dotted] {
                close_container(0, handle);
            }
            close_provider(0, provider);
        }
    }

    #[test]
    fn the_quota_and_the_limit_on_one_update_are_kept() {
        let folder = Folder::new("limits");
        let provider = folder.open(1000);
        let (one, two) = (container(provider, "one"), container(provider, "two"));
        assert_eq!(quota(provider), 1000);
        assert_eq!(update(one, None, &[("blob", &[1; 600])], &[], false), S_OK);
        assert_eq!(quota(provider), 400);
        // Past the quota by either form, and nothing of the refused update is there.
        for asynchronous in [false, true] {
            assert_eq!(
                update(two, None, &[("blob", &[2; 401])], &[], asynchronous),
                QUOTA_EXCEEDED
            );
        }
        assert!(blob_list(two, None).is_empty());
        assert_eq!(containers(provider, None).len(), 1);
        assert_eq!(update(two, None, &[("blob", &[2; 400])], &[], false), S_OK);
        assert_eq!(quota(provider), 0);
        // A full quota still takes an update that replaces a blob with one no larger.
        assert_eq!(update(one, None, &[("blob", &[3; 600])], &[], false), S_OK);
        assert_eq!(
            update(one, None, &[("more", &[3; 1])], &[], false),
            QUOTA_EXCEEDED
        );
        assert_eq!(
            read(one, None, 4096, false),
            Ok(named(&[("blob", &[3; 600])]))
        );

        // One blob over the limit is refused when it is added; several that pass it
        // together when the update is submitted.
        let limit = store::UPDATE_LIMIT as usize;
        let big = vec![0u8; limit + 1];
        let mut handle = std::ptr::null_mut();
        // SAFETY: live handles, strings, and buffers of the sizes given.
        unsafe {
            close_provider(0, provider);
            // The containers outlive their provider; this one has room for the big update.
            let provider = folder.open(QUOTA);
            let large = container(provider, "large");
            assert_eq!(create_update(0, large, std::ptr::null(), &mut handle), S_OK);
            assert_eq!(
                submit_blob_write(0, handle, c("a").as_ptr(), big.as_ptr(), limit + 1),
                UPDATE_TOO_BIG
            );
            for name in ["a", "b"] {
                assert_eq!(
                    submit_blob_write(0, handle, c(name).as_ptr(), big.as_ptr(), limit / 2 + 1),
                    S_OK
                );
            }
            assert_eq!(submit_update(0, handle), UPDATE_TOO_BIG);
            let mut block = Block::new();
            assert_eq!(submit_update_async(0, handle, block.get()), S_OK);
            assert_eq!(submit_update_result(0, block.get()), UPDATE_TOO_BIG);
            close_update(0, handle);
            assert!(blob_list(large, None).is_empty());
            assert_eq!(containers(provider, None).len(), 2);
            for handle in [one, two, large] {
                close_container(0, handle);
            }
            close_provider(0, provider);
        }
    }

    #[test]
    fn the_asynchronous_forms_give_what_the_blocking_ones_give() {
        let folder = Folder::new("async");
        let provider = folder.open(QUOTA);
        let (blocking, asynchronous) = (
            container(provider, "blocking"),
            container(provider, "async"),
        );
        let first: [(&str, &[u8]); 3] = [("a", b"one"), ("b", b"two"), ("c", &[9; 500])];
        let second: [(&str, &[u8]); 1] = [("b", b"two again")];
        for (slot, by_block) in [(blocking, false), (asynchronous, true)] {
            assert_eq!(update(slot, Some("Same"), &first, &[], by_block), S_OK);
            assert_eq!(update(slot, None, &second, &["a"], by_block), S_OK);
        }
        let after: [(&str, &[u8]); 2] = [("b", b"two again"), ("c", &[9; 500])];
        for slot in [blocking, asynchronous] {
            assert_eq!(
                blob_list(slot, None),
                [("b".to_owned(), 9), ("c".to_owned(), 500)]
            );
            for by_block in [false, true] {
                assert_eq!(read(slot, None, 4096, by_block), Ok(named(&after)));
                assert_eq!(
                    read(slot, Some(&["c"]), 4096, by_block),
                    Ok(named(&after[1..]))
                );
                assert_eq!(
                    read(slot, Some(&["a"]), 4096, by_block),
                    Err(BLOB_NOT_FOUND)
                );
                assert_eq!(read(slot, Some(&[]), 4096, by_block), Ok(Vec::new()));
                assert_eq!(read(slot, None, 100, by_block), Err(BUFFER_TOO_SMALL));
            }
        }
        let found = containers(provider, None);
        assert_eq!(found[0].1, found[1].1);
        assert_eq!((found[0].2, found[0].3), (found[1].2, found[1].3));
        assert_eq!((found[0].2, found[0].3), (2, 509));

        let mut block = Block::new();
        let (mut left, mut size) = (0i64, 0);
        // SAFETY: live handles, live blocks, and valid out-pointers; the null ones are
        // under test.
        unsafe {
            assert_eq!(remaining_quota_async(0, provider, block.get()), S_OK);
            let sized = xasync::get_result_size(std::ptr::null_mut(), block.get(), &mut size);
            assert_eq!((sized, size), (S_OK, 8));
            assert_eq!(
                remaining_quota_result(0, block.get(), std::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(remaining_quota_result(0, block.get(), &mut left), S_OK);
            assert_eq!(left, quota(provider));
            assert_eq!(left, QUOTA as i64 - 2 * 509);

            // A call that cannot begin its work completes with the failure.
            assert_eq!(
                remaining_quota_async(0, std::ptr::null(), block.get()),
                S_OK
            );
            assert_eq!(
                remaining_quota_result(0, block.get(), &mut left),
                E_INVALIDARG
            );
            assert_eq!(submit_update_async(0, std::ptr::null(), block.get()), S_OK);
            assert_eq!(submit_update_result(0, block.get()), E_INVALIDARG);
            let begun = delete_container_async(0, provider, c("../x").as_ptr(), block.get());
            assert_eq!(begun, S_OK);
            assert_eq!(
                delete_container_result(0, block.get()),
                INVALID_CONTAINER_NAME
            );
            assert_eq!(
                delete_container_async(0, provider, c("x").as_ptr(), std::ptr::null_mut()),
                E_INVALIDARG
            );

            assert_eq!(
                delete_container_async(0, provider, c("async").as_ptr(), block.get()),
                S_OK
            );
            assert_eq!(delete_container_result(0, block.get()), S_OK);
        }
        assert_eq!(containers(provider, None).len(), 1);
        assert_eq!(read(asynchronous, None, 4096, true), Err(BLOB_NOT_FOUND));

        let mut out = std::ptr::null_mut();
        // SAFETY: null arguments, which are refused before anything is touched; then
        // handles closed once.
        unsafe {
            assert_eq!(
                initialize(0, std::ptr::null(), std::ptr::null(), 0, &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                initialize(
                    0,
                    std::ptr::null(),
                    c("id").as_ptr(),
                    0,
                    std::ptr::null_mut()
                ),
                E_INVALIDARG
            );
            for handle in [blocking, asynchronous] {
                close_container(0, handle);
            }
            close_provider(0, provider);
        }
    }

    #[test]
    fn what_an_interrupted_save_left_is_not_read_and_is_gone_after_the_next_open() {
        let folder = Folder::new("interrupted");
        let provider = folder.open(QUOTA);
        let slot = container(provider, "slot");
        assert_eq!(
            update(
                slot,
                Some("Good"),
                &[("blob", b"the good save")],
                &[],
                false
            ),
            S_OK
        );

        // What a process that died while writing the next version leaves: a newer version
        // with half a blob and no index, and a half-written pointer to it.
        let saves = folder.0.join("saves");
        let stored: Vec<_> = std::fs::read_dir(&saves).unwrap().flatten().collect();
        assert_eq!(stored.len(), 1);
        let stored = stored[0].path();
        std::fs::create_dir_all(stored.join("gen-2")).unwrap();
        std::fs::write(stored.join("gen-2").join("b0"), b"half of the ne").unwrap();
        std::fs::write(stored.join("live.saving"), b"").unwrap();
        // And a container whose first save never finished.
        let unfinished = saves.join("c0123456789abcdef-unfinished");
        std::fs::create_dir_all(unfinished.join("gen-1")).unwrap();
        std::fs::write(unfinished.join("gen-1").join("b0"), b"half").unwrap();

        let good = named(&[("blob", b"the good save")]);
        let listed = [("slot".to_owned(), "Good".to_owned(), 1, 13)];
        assert_eq!(read(slot, None, 4096, false), Ok(good.clone()));
        assert_eq!(containers(provider, None), listed);
        assert_eq!(quota(provider), QUOTA as i64 - 13);
        // SAFETY: handles closed once.
        unsafe {
            close_container(0, slot);
            close_provider(0, provider);
        }

        let provider = folder.open(QUOTA);
        let slot = container(provider, "slot");
        assert!(!stored.join("gen-2").exists());
        assert!(!stored.join("live.saving").exists());
        assert!(!unfinished.exists());
        assert_eq!(read(slot, None, 4096, false), Ok(good));
        assert_eq!(containers(provider, None), listed);
        // The next save goes through as if nothing had happened.
        assert_eq!(update(slot, None, &[("blob", b"newer")], &[], true), S_OK);
        assert_eq!(
            read(slot, None, 4096, true),
            Ok(named(&[("blob", b"newer")]))
        );
        // SAFETY: handles closed once.
        unsafe {
            close_container(0, slot);
            close_provider(0, provider);
        }
    }

    #[test]
    fn every_built_method_has_its_own_position() {
        let methods = methods();
        let positions: Vec<usize> = methods.iter().map(|method| method.0).collect();
        assert_eq!(positions, (3..=29).collect::<Vec<_>>());
    }
}
