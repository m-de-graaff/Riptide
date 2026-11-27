//! Reading the machine's facts from Windows.

use crate::Machine;
use riptide_config::Settings;
use std::path::Path;
use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Power::PowerGetActiveScheme;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::core::{GUID, Interface};

/// The "Power saver" plan's identifier.
const POWER_SAVER: GUID = GUID::from_u128(0xa1841308_3541_4fab_bc81_f71556f20b4a);
const MIB: u64 = 1 << 20;

pub fn gather(settings: &Settings, runtime: &Path) -> Machine {
    let (free_ram_mib, commit_headroom_mib) = memory();
    Machine {
        free_ram_mib,
        commit_headroom_mib,
        gpu: gpu(),
        power_saver: power_saver(),
        game_folder_exists: settings.game.folder.as_ref().map(|f| f.is_dir()),
        runtime_present: runtime.is_file(),
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

/// The hardware adapter with the most video memory, as "name (driver a.b.c.d)".
fn gpu() -> Option<String> {
    // SAFETY: plain COM calls; every interface is released by its smart pointer.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let mut best: Option<(usize, String)> = None;
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
                .is_some_and(|(memory, _)| *memory >= desc.DedicatedVideoMemory)
            {
                continue;
            }
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            let label = match adapter.CheckInterfaceSupport(&IDXGIDevice::IID) {
                Ok(v) => {
                    let w = |shift: u32| (v >> shift) & 0xFFFF;
                    format!("{name} (driver {}.{}.{}.{})", w(48), w(32), w(16), w(0))
                }
                Err(_) => name,
            };
            best = Some((desc.DedicatedVideoMemory, label));
        }
        best.map(|(_, label)| label)
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
