//! Reading the machine's facts from Windows.

use crate::{Drive, Driver, Machine, real_time_scanning};
use riptide_config::Settings;
use std::path::{Component, Path, Prefix};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HLOCAL, LocalFree};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW,
    OPEN_EXISTING,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
};
use windows::Win32::System::Power::PowerGetActiveScheme;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
use windows::Win32::System::Wmi::{
    IWbemLocator, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE, WbemLocator,
};
use windows::core::{BSTR, GUID, HSTRING, Interface, PCWSTR, w};

/// The "Power saver" plan's identifier.
const POWER_SAVER: GUID = GUID::from_u128(0xa1841308_3541_4fab_bc81_f71556f20b4a);
const MIB: u64 = 1 << 20;

/// `RPC_C_AUTHN_WINNT` and `RPC_C_AUTHZ_NONE`: Windows' own sign-on, no extra authorisation.
const AUTHN_WINNT: u32 = 10;
const AUTHZ_NONE: u32 = 0;

pub fn gather(settings: &Settings, runtime: &Path) -> Machine {
    let (free_ram_mib, commit_headroom_mib) = memory();
    let (gpu, gpu_driver) = match gpu() {
        Some((label, driver)) => (Some(label), driver),
        None => (None, None),
    };
    let folder = settings.game.folder.as_ref();
    Machine {
        free_ram_mib,
        commit_headroom_mib,
        gpu,
        power_saver: power_saver(),
        game_folder_exists: folder.map(|f| f.is_dir()),
        runtime_present: runtime.is_file(),
        gpu_driver,
        game_drive: folder.filter(|f| f.is_dir()).and_then(|f| drive(f)),
        scanners: scanners(),
        cache_folder: cache_folder(),
    }
}

/// Where Riptide keeps its caches and logs: `%LOCALAPPDATA%\Riptide`.
fn cache_folder() -> String {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) => Path::new(&local).join("Riptide").display().to_string(),
        None => r"%LOCALAPPDATA%\Riptide".to_owned(),
    }
}

/// Free physical memory and remaining commit, in MiB. Zeros if Windows will not say.
fn memory() -> (u64, u64) {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set, as the call requires.
    match unsafe { GlobalMemoryStatusEx(&mut status) } {
        Ok(()) => (status.ullAvailPhys / MIB, status.ullAvailPageFile / MIB),
        Err(_) => (0, 0),
    }
}

/// The hardware adapter with the most video memory, as "name (driver a.b.c.d)", and its
/// driver if Windows gives the version.
fn gpu() -> Option<(String, Option<Driver>)> {
    // SAFETY: plain COM calls; every interface is released by its smart pointer.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let mut best: Option<(usize, String, Option<Driver>)> = None;
        let mut i = 0;
        while let Ok(adapter) = factory
            .EnumAdapterByGpuPreference::<IDXGIAdapter4>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
        {
            i += 1;
            let Ok(desc) = adapter.GetDesc3() else {
                continue;
            };
            if (desc.Flags.0 & DXGI_ADAPTER_FLAG3_SOFTWARE.0) != 0 {
                continue;
            }
            if best
                .as_ref()
                .is_some_and(|(memory, _, _)| *memory >= desc.DedicatedVideoMemory)
            {
                continue;
            }
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            let driver = adapter
                .CheckInterfaceSupport(&IDXGIDevice::IID)
                .ok()
                .map(|v| {
                    let w = |shift: u32| ((v >> shift) & 0xFFFF) as u16;
                    Driver {
                        vendor_id: desc.VendorId,
                        version: [w(48), w(32), w(16), w(0)],
                    }
                });
            let label = match driver {
                Some(driver) => format!("{name} (driver {driver})"),
                None => name,
            };
            best = Some((desc.DedicatedVideoMemory, label, driver));
        }
        best.map(|(_, label, driver)| (label, driver))
    }
}

fn power_saver() -> Option<bool> {
    let mut scheme: *mut GUID = std::ptr::null_mut();
    // SAFETY: `scheme` is a valid out-pointer. On success Windows allocates the GUID, which
    // is read once and then freed with LocalFree, as the API documents.
    unsafe {
        if PowerGetActiveScheme(None, &mut scheme) != ERROR_SUCCESS || scheme.is_null() {
            return None;
        }
        let active = *scheme;
        let _ = LocalFree(Some(HLOCAL(scheme.cast())));
        Some(active == POWER_SAVER)
    }
}

/// Free space on the drive `folder` is on, and whether that drive is a hard disk.
fn drive(folder: &Path) -> Option<Drive> {
    let mut free = 0u64;
    // SAFETY: the path is null-terminated by HSTRING and `free` is a valid out-pointer.
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(folder), Some(&mut free), None, None) }.ok()?;
    Some(Drive {
        free_mib: free / MIB,
        seek_penalty: seek_penalty(folder),
    })
}

/// Asks the drive under `folder` whether its reads pay a seek penalty. `None` for a folder
/// that is not on a lettered drive (a network share), or a drive that will not answer.
fn seek_penalty(folder: &Path) -> Option<bool> {
    let absolute = std::path::absolute(folder).ok()?;
    let letter = match absolute.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter as char,
            _ => return None,
        },
        _ => return None,
    };
    let volume = HSTRING::from(format!(r"\\.\{letter}:"));
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut answer = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0u32;
    // SAFETY: the volume is opened with no access rights, which is enough to query it and
    // needs no elevation; the handle is closed before returning. The query and answer
    // buffers are valid for the sizes given.
    unsafe {
        let handle = CreateFileW(
            &volume,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .ok()?;
        let asked = DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast()),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some((&raw mut answer).cast()),
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            Some(&mut returned),
            None,
        );
        let _ = CloseHandle(handle);
        asked.ok()?;
    }
    (returned as usize >= std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>())
        .then_some(answer.IncursSeekPenalty)
}

/// The antivirus products whose real-time scanning is on, as Windows' Security Center lists
/// them. `None` if it cannot be asked. Security Center exists on desktop Windows, not on
/// Windows Server.
fn scanners() -> Option<Vec<String>> {
    // SAFETY: COM is initialised for this thread first. A thread that already has COM in
    // another mode refuses the call and COM stays usable, so only a call that succeeded is
    // paired with CoUninitialize, after every interface is released.
    unsafe {
        let initialised = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let names = query_scanners();
        if initialised {
            CoUninitialize();
        }
        names
    }
}

/// # Safety
/// COM is initialised on this thread.
unsafe fn query_scanners() -> Option<Vec<String>> {
    // SAFETY: plain COM calls; every interface is released by its smart pointer, and each
    // VARIANT frees what it holds when dropped.
    unsafe {
        let locator: IWbemLocator =
            CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
        let services = locator
            .ConnectServer(
                &BSTR::from(r"ROOT\SecurityCenter2"),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None,
            )
            .ok()?;
        CoSetProxyBlanket(
            &services,
            AUTHN_WINNT,
            AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
        .ok()?;
        let rows = services
            .ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT displayName, productState FROM AntiVirusProduct"),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
            .ok()?;

        let mut names: Vec<String> = Vec::new();
        loop {
            let mut row = [None];
            let mut returned = 0u32;
            if rows.Next(WBEM_INFINITE, &mut row, &mut returned).is_err() || returned == 0 {
                break;
            }
            let Some(product) = row[0].take() else {
                break;
            };
            let mut name = VARIANT::default();
            let mut state = VARIANT::default();
            if product
                .Get(w!("displayName"), 0, &mut name, None, None)
                .is_err()
                || product
                    .Get(w!("productState"), 0, &mut state, None, None)
                    .is_err()
            {
                continue;
            }
            // A VARIANT is a tagged union; the name is read only when its tag says string.
            let inner = &name.Anonymous.Anonymous;
            if inner.vt != VT_BSTR {
                continue;
            }
            let name = inner.Anonymous.bstrVal.to_string();
            let Ok(state) = i32::try_from(&state) else {
                continue;
            };
            // One product can be listed several times.
            if real_time_scanning(state as u32) && !names.contains(&name) {
                names.push(name);
            }
        }
        Some(names)
    }
}
