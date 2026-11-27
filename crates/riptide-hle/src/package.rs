//! The runtime's package area (`XPackage`): the title's own package and what is installed
//! of it.
//!
//! Built so far: only what the real title has called. The whole package is always installed:
//! the game folder is all of it.

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

use crate::trace::trace;

/// The interface id a title asks for: `0b9d556e-ab70-43f4-baa0-9999ac4cb040`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x6e, 0x55, 0x9d, 0x0b, 0x70, 0xab, 0xf4, 0x43, 0xba, 0xa0, 0x99, 0x99, 0xac, 0x4c, 0xb0, 0x40,
];

pub(crate) const NAME: &str = "packages";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
/// `ERROR_FILE_NOT_FOUND` as a result code.
const E_FILE_NOT_FOUND: i32 = 0x8007_0002_u32 as i32;
/// `ERROR_INSUFFICIENT_BUFFER` as a result code.
const E_INSUFFICIENT_BUFFER: i32 = 0x8007_007A_u32 as i32;

/// The longest package identifier, without its terminator (`XPACKAGE_IDENTIFIER_MAX_LENGTH`
/// is 33 with it).
const IDENTIFIER_MAX: usize = 32;

/// The value of attribute `name` of the first `<element ...>` in `xml`.
///
/// The package's configuration file is small and regular, so this looks for the text and
/// does not parse XML.
pub(crate) fn attribute<'a>(xml: &'a str, element: &str, name: &str) -> Option<&'a str> {
    let open = format!("<{element}");
    let mut from = 0;
    let tag = loop {
        let start = from + xml[from..].find(&open)?;
        let rest = &xml[start + open.len()..];
        // `<Identity` must not match `<IdentityOther`.
        if rest.starts_with(|c: char| c.is_whitespace()) {
            break &rest[..rest.find('>')?];
        }
        from = start + open.len();
    };
    let mut rest = tag;
    loop {
        let at = rest.find(name)?;
        let before_ok = rest[..at].ends_with(|c: char| c.is_whitespace());
        let after = rest[at + name.len()..].trim_start();
        if before_ok && let Some(value) = after.strip_prefix('=') {
            let value = value.trim_start();
            let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
            let value = &value[1..];
            return Some(&value[..value.find(quote)?]);
        }
        rest = &rest[at + name.len()..];
    }
}

/// The package's configuration file, `MicrosoftGame.Config` in the game folder, read once.
pub(crate) fn config() -> Option<&'static str> {
    static CONFIG: OnceLock<Option<String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let path = crate::process::title_folder()?.join("MicrosoftGame.Config");
            let text = std::fs::read_to_string(&path).ok();
            if text.is_none() {
                trace!("packages: {} cannot be read", path.display());
            }
            text
        })
        .as_deref()
}

/// The package's identifier: its identity name from the configuration file, cut to the
/// length the console allows.
fn identifier(config: &str) -> Option<&str> {
    let name = attribute(config, "Identity", "Name")?;
    let mut end = name.len().min(IDENTIFIER_MAX);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    Some(&name[..end])
}

/// Copies `text` and a terminator into the title's buffer.
///
/// # Safety
/// `buffer` is null or has room for `size` bytes.
pub(crate) unsafe fn copy_text(text: &str, size: usize, buffer: *mut u8) -> i32 {
    if buffer.is_null() {
        return E_INVALIDARG;
    }
    if size < text.len() + 1 {
        return E_INSUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer has room for the text and its terminator, checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
        buffer.add(text.len()).write(0);
    }
    S_OK
}

/// `XPackageGetCurrentProcessPackageIdentifier`: the identifier of the package the title
/// runs from.
///
/// # Safety
/// `buffer` is null or has room for `size` bytes.
unsafe extern "system" fn current_process_package_identifier(
    _this: usize,
    size: usize,
    buffer: *mut u8,
) -> i32 {
    let Some(identifier) = config().and_then(identifier) else {
        trace!("XPackageGetCurrentProcessPackageIdentifier: the package has no identity");
        return E_FILE_NOT_FOUND;
    };
    // SAFETY: the title's buffer, as it describes it.
    let result = unsafe { copy_text(identifier, size, buffer) };
    trace!(
        "XPackageGetCurrentProcessPackageIdentifier = {identifier} ({:#010x})",
        result as u32
    );
    result
}

/// `XPackageIsPackagedProcess`: whether the title runs from an installed package and not
/// loose from a development folder. It does: its files are read-only on the package drive.
extern "system" fn is_packaged_process(_this: usize) -> bool {
    true
}

/// What a title holds while it watches an installation. Nothing is ever being installed,
/// so it has nothing in it; it exists so each monitor is a handle of its own.
struct Monitor;

/// `XPackageInstallationProgress`: how much of what a monitor watches is installed.
#[repr(C)]
#[derive(Debug, PartialEq, Eq)]
struct Progress {
    total_bytes: u64,
    installed_bytes: u64,
    launch_bytes: u64,
    launchable: bool,
    completed: bool,
}

/// `XPackageCreateInstallationMonitor`: a handle that reports the installation of some
/// chunks of a package: `(package, selector count, selectors, minimum update interval,
/// queue, monitor out)`.
///
/// # Safety
/// `monitor` is null or where the handle goes.
unsafe extern "system" fn create_installation_monitor(
    _this: usize,
    _package: *const u8,
    selectors: u32,
    _selector_list: *const u8,
    _interval_ms: u32,
    _queue: usize,
    monitor: *mut usize,
) -> i32 {
    if monitor.is_null() {
        return E_INVALIDARG;
    }
    let handle = Box::into_raw(Box::new(Monitor)) as usize;
    // `Monitor` has no size, so every box of it has the same address: fine for a handle
    // that carries nothing, and `Box::from_raw` on it frees nothing.
    // SAFETY: checked non-null.
    unsafe { monitor.write(handle) };
    trace!("XPackageCreateInstallationMonitor({selectors} selectors): all of it is installed");
    S_OK
}

/// `XPackageCloseInstallationMonitorHandle`.
extern "system" fn close_installation_monitor(_this: usize, _monitor: usize) {}

/// `XPackageGetInstallationProgress`: everything is installed. The package's size in bytes
/// is not known here, so the counts say "one of one".
///
/// # Safety
/// `progress` is null or where the progress goes.
unsafe extern "system" fn installation_progress(
    _this: usize,
    _monitor: usize,
    progress: *mut Progress,
) {
    if !progress.is_null() {
        // SAFETY: checked non-null.
        unsafe {
            progress.write(Progress {
                total_bytes: 1,
                installed_bytes: 1,
                launch_bytes: 1,
                launchable: true,
                completed: true,
            })
        };
    }
}

/// `XPackageUpdateInstallationMonitor`: whether the progress changed since it was last
/// asked. It never does.
extern "system" fn update_installation_monitor(_this: usize, _monitor: usize) -> bool {
    false
}

/// `XPackageUnregisterInstallationProgressChanged`: `(monitor, token, wait)`.
extern "system" fn unregister_progress_changed(
    _this: usize,
    _monitor: usize,
    _token: u64,
    _wait: bool,
) -> bool {
    true
}

/// `XPackageFindChunkAvailability`: whether some chunks of a package are installed:
/// `(package, selector count, selectors, availability out)`. They are: 0 is "ready".
///
/// # Safety
/// `availability` is null or where the answer goes.
unsafe extern "system" fn find_chunk_availability(
    _this: usize,
    _package: *const u8,
    _selectors: u32,
    _selector_list: *const u8,
    availability: *mut u32,
) -> i32 {
    if availability.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { availability.write(0) };
    S_OK
}

/// `XVersion`: a package's version, as four numbers.
#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
struct Version {
    major: u16,
    minor: u16,
    build: u16,
    revision: u16,
}

/// A version written `1.2.3.4`. Parts that are missing or not numbers are 0.
fn version(text: &str) -> Version {
    let mut parts = text.split('.').map(|part| part.trim().parse().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    Version {
        major: next(),
        minor: next(),
        build: next(),
        revision: next(),
    }
}

/// `XPackageDetails`: one installed package, as a title is told of it.
#[repr(C)]
struct Details {
    identifier: *const c_char,
    version: Version,
    /// 0 is a game, 1 is additional content.
    kind: u32,
    display_name: *const c_char,
    description: *const c_char,
    publisher: *const c_char,
    store_id: *const c_char,
    installing: bool,
    index: u32,
    count: u32,
    age_restricted: bool,
    title_id: *const c_char,
}

/// The strings [`Details`] points at for the title's own package.
struct Own {
    identifier: CString,
    version: Version,
    display_name: CString,
    description: CString,
    publisher: CString,
    store_id: CString,
    title_id: CString,
}

/// The text between `<element>` and `</element>`.
pub(crate) fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{name}>"))?;
    Some(xml[start..end].trim())
}

/// The title's own package, as its configuration file describes it.
fn own(config: &str) -> Option<Own> {
    let text = |value: Option<&str>| {
        CString::new(value.unwrap_or_default().replace('\0', "")).expect("no null bytes")
    };
    Some(Own {
        identifier: text(Some(identifier(config)?)),
        version: version(attribute(config, "Identity", "Version").unwrap_or_default()),
        display_name: text(attribute(config, "ShellVisuals", "DefaultDisplayName")),
        description: text(attribute(config, "ShellVisuals", "Description")),
        publisher: text(attribute(config, "Identity", "Publisher")),
        store_id: text(element(config, "StoreId")),
        title_id: text(element(config, "TitleId")),
    })
}

/// The callback `XPackageEnumeratePackages` calls for each package; false stops the list.
type PackageCallback = unsafe extern "system" fn(context: usize, details: *const Details) -> bool;

/// `XPackageEnumeratePackages`: the installed packages of one kind: `(kind, scope, context,
/// callback)`. The only package is the title's own, a game; no additional content is
/// installed.
///
/// # Safety
/// `callback` is null or a function of the title's with the callback's signature.
unsafe extern "system" fn enumerate_packages(
    _this: usize,
    kind: u32,
    scope: u32,
    context: usize,
    callback: Option<PackageCallback>,
) -> i32 {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    if kind != 0 {
        trace!("XPackageEnumeratePackages(kind {kind}, scope {scope}): none");
        return S_OK;
    }
    let Some(own) = config().and_then(own) else {
        trace!("XPackageEnumeratePackages: the package has no identity");
        return E_FILE_NOT_FOUND;
    };
    trace!("XPackageEnumeratePackages(kind {kind}, scope {scope}): the title's own");
    let details = Details {
        identifier: own.identifier.as_ptr(),
        version: own.version,
        kind: 0,
        display_name: own.display_name.as_ptr(),
        description: own.description.as_ptr(),
        publisher: own.publisher.as_ptr(),
        store_id: own.store_id.as_ptr(),
        installing: false,
        index: 0,
        count: 1,
        age_restricted: false,
        title_id: own.title_id.as_ptr(),
    };
    // SAFETY: the title's callback, called as the console calls it; the strings live until
    // it returns, which is as long as the console promises.
    unsafe { callback(context, &details) };
    S_OK
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        // +0x18 `XPackageGetCurrentProcessPackageIdentifier`.
        (3, current_process_package_identifier as *const () as usize),
        // +0x20 `XPackageIsPackagedProcess`.
        (4, is_packaged_process as *const () as usize),
        // +0x28 to +0x40: installation monitors.
        (5, create_installation_monitor as *const () as usize),
        (6, close_installation_monitor as *const () as usize),
        (7, installation_progress as *const () as usize),
        (8, update_installation_monitor as *const () as usize),
        // +0x48 and +0x50 `XPackageRegisterInstallationProgressChanged`: `(monitor, context,
        // callback, token)`. Progress never changes, so the callback is never called.
        (9, crate::com::register_for_nothing as *const () as usize),
        (10, unregister_progress_changed as *const () as usize),
        // +0x60 `XPackageFindChunkAvailability`.
        (12, find_chunk_availability as *const () as usize),
        // +0x138 `XPackageEnumeratePackages`.
        (39, enumerate_packages as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="1">
  <TitleId>0A1B2C3D</TitleId>
  <StoreId>9ABCDEFGHIJK</StoreId>
  <ShellVisuals DefaultDisplayName="Some Game" Description="A game" />
  <IdentityOther Name="wrong" />
  <Identity Publisher="CN=Someone" Name = "Maker.SomeGameWithAVeryLongNameIndeed0123" Version='1.2.3.4' />
</Game>"#;

    #[test]
    fn an_attribute_is_found_by_element_and_name() {
        assert_eq!(
            attribute(CONFIG, "Identity", "Name"),
            Some("Maker.SomeGameWithAVeryLongNameIndeed0123")
        );
        assert_eq!(attribute(CONFIG, "Identity", "Version"), Some("1.2.3.4"));
        assert_eq!(attribute(CONFIG, "Game", "configVersion"), Some("1"));
        assert_eq!(attribute(CONFIG, "Identity", "Missing"), None);
        assert_eq!(attribute(CONFIG, "Missing", "Name"), None);
    }

    #[test]
    fn the_identifier_is_the_identity_name_cut_to_32_bytes() {
        assert_eq!(identifier(CONFIG), Some("Maker.SomeGameWithAVeryLongNameI"));
        assert_eq!(identifier("<Game />"), None);
    }

    #[test]
    fn text_is_copied_with_its_terminator_or_refused() {
        let mut buffer = [0xFFu8; 8];
        // SAFETY: the buffer is as long as the size given.
        unsafe {
            assert_eq!(copy_text("abc", 8, buffer.as_mut_ptr()), S_OK);
            assert_eq!(&buffer[..4], b"abc\0");
            assert_eq!(
                copy_text("abc", 3, buffer.as_mut_ptr()),
                E_INSUFFICIENT_BUFFER
            );
            assert_eq!(copy_text("abc", 8, std::ptr::null_mut()), E_INVALIDARG);
        }
    }

    #[test]
    fn a_monitor_reports_everything_installed() {
        let mut monitor = 0usize;
        let mut progress = Progress {
            total_bytes: 0,
            installed_bytes: 0,
            launch_bytes: 0,
            launchable: false,
            completed: false,
        };
        let mut availability = 9u32;
        // SAFETY: valid out-pointers.
        unsafe {
            let made = create_installation_monitor(
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                &mut monitor,
            );
            assert_eq!(made, S_OK);
            assert_ne!(monitor, 0);
            installation_progress(0, monitor, &mut progress);
            assert_eq!(
                find_chunk_availability(
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    &mut availability
                ),
                S_OK
            );
        }
        assert!(progress.completed && progress.launchable);
        assert_eq!(progress.installed_bytes, progress.total_bytes);
        assert_eq!(availability, 0);
        assert!(!update_installation_monitor(0, monitor));
        close_installation_monitor(0, monitor);
    }

    #[test]
    fn a_version_is_four_numbers() {
        let parsed = version("1.0.2.1");
        assert_eq!(
            (parsed.major, parsed.minor, parsed.build, parsed.revision),
            (1, 0, 2, 1)
        );
        assert_eq!(
            version("3.x"),
            Version {
                major: 3,
                ..Version::default()
            }
        );
    }

    #[test]
    fn the_details_have_the_consoles_layout() {
        assert_eq!(std::mem::size_of::<Details>(), 80);
        assert_eq!(std::mem::offset_of!(Details, kind), 16);
        assert_eq!(std::mem::offset_of!(Details, display_name), 24);
        assert_eq!(std::mem::offset_of!(Details, installing), 56);
        assert_eq!(std::mem::offset_of!(Details, index), 60);
        assert_eq!(std::mem::offset_of!(Details, age_restricted), 68);
        assert_eq!(std::mem::offset_of!(Details, title_id), 72);
    }

    #[test]
    fn the_own_package_is_read_from_the_configuration() {
        let own = own(CONFIG).unwrap();
        assert_eq!(
            own.identifier.to_str().unwrap(),
            "Maker.SomeGameWithAVeryLongNameI"
        );
        assert_eq!(
            own.version,
            Version {
                major: 1,
                minor: 2,
                build: 3,
                revision: 4
            }
        );
        assert_eq!(own.display_name.to_str().unwrap(), "Some Game");
        assert_eq!(own.description.to_str().unwrap(), "A game");
        assert_eq!(own.publisher.to_str().unwrap(), "CN=Someone");
        assert_eq!(own.store_id.to_str().unwrap(), "9ABCDEFGHIJK");
        assert_eq!(own.title_id.to_str().unwrap(), "0A1B2C3D");
    }

    #[test]
    fn the_title_runs_from_a_package() {
        assert!(is_packaged_process(0));
    }
}
