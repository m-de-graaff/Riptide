//! The host's D3D12 device: the real graphics card every console graphics object is built
//! on.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_1;
use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    IDXGIAdapter1, IDXGIFactory6,
};

use crate::object::Hresult;
use crate::trace;

/// The host device and what it runs on.
pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(crate) adapter: String,
}

// SAFETY: D3D12 devices are free-threaded: any thread may call them at any time.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Whether the software adapter was asked for (`RIPTIDE_ADAPTER=software`): for tests on
/// machines without a graphics card, never for running a title.
fn software() -> bool {
    std::env::var_os("RIPTIDE_ADAPTER").is_some_and(|value| value == "software")
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain calls; every out-pointer is valid.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter1 = if software() {
            factory.EnumWarpAdapter()?
        } else {
            factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)?
        };
        let description = adapter.GetDesc1()?;
        let length = description
            .Description
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(description.Description.len());
        let name = String::from_utf16_lossy(&description.Description[..length]);
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_1, &mut device)?;
        let device =
            device.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        Ok(Host {
            device,
            adapter: name,
        })
    }
}

/// The host device, made the first time it is asked for. An error is the host's own result
/// code, and is given again each time.
pub(crate) fn host() -> Result<&'static Host, Hresult> {
    static HOST: OnceLock<Result<Host, Hresult>> = OnceLock::new();
    HOST.get_or_init(|| {
        create()
            .inspect(|host| trace!("host graphics device made on {}", host.adapter))
            .map_err(|error| {
                trace!("no host graphics device: {error}");
                error.code().0
            })
    })
    .as_ref()
    .map_err(|&code| code)
}
