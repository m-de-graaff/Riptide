//! The D3D12 device on the target GPU, its feature answers, and one-shot command submission.

use crate::report::Facts;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_1;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[derive(Clone, Debug)]
pub struct AdapterInfo {
    pub name: String,
    pub dedicated_video_memory: u64,
    pub software: bool,
}

/// The hardware adapter with the most dedicated video memory.
pub fn pick_adapter(adapters: &[AdapterInfo]) -> Option<usize> {
    adapters
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.software)
        .max_by_key(|(_, a)| a.dedicated_video_memory)
        .map(|(i, _)| i)
}

/// The user-mode driver version DXGI reports, as four 16-bit words.
pub fn format_umd_version(raw: i64) -> String {
    let w = |shift: u32| (raw >> shift) & 0xFFFF;
    format!("{}.{}.{}.{}", w(48), w(32), w(16), w(0))
}

pub struct Gpu {
    pub info: AdapterInfo,
    pub driver_version: String,
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
}

pub fn open(debug: bool) -> windows::core::Result<Gpu> {
    // SAFETY: plain COM calls with valid out-pointers; every returned interface is owned by
    // a windows-crate smart pointer that releases it.
    unsafe {
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            D3D12GetDebugInterface(&mut layer)?;
            if let Some(layer) = layer {
                layer.EnableDebugLayer();
            }
        }
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let mut adapters = Vec::new();
        let mut infos = Vec::new();
        let mut i = 0;
        while let Ok(adapter) =
            factory.EnumAdapterByGpuPreference::<IDXGIAdapter4>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
        {
            let d = adapter.GetDesc3()?;
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string();
            infos.push(AdapterInfo {
                name,
                dedicated_video_memory: d.DedicatedVideoMemory as u64,
                software: (d.Flags.0 & DXGI_ADAPTER_FLAG3_SOFTWARE.0) != 0,
            });
            adapters.push(adapter);
            i += 1;
        }
        let index = pick_adapter(&infos)
            .ok_or_else(|| windows::core::Error::new(DXGI_ERROR_NOT_FOUND, "no hardware adapter"))?;
        let adapter = &adapters[index];
        let driver_version = adapter
            .CheckInterfaceSupport(&IDXGIDevice::IID)
            .map(format_umd_version)
            .unwrap_or_else(|e| format!("unknown ({e})"));
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_1, &mut device)?;
        let device = device.expect("D3D12CreateDevice succeeded without a device");
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        Ok(Gpu { info: infos[index].clone(), driver_version, device, queue })
    }
}

fn feature<T: Default>(gpu: &Gpu, id: D3D12_FEATURE) -> windows::core::Result<T> {
    let mut data = T::default();
    // SAFETY: `data` is a writable `T` and the size passed is exactly `size_of::<T>()`; each
    // caller pairs `id` with its matching D3D12_FEATURE_DATA_* type.
    unsafe {
        gpu.device.CheckFeatureSupport(id, &mut data as *mut T as *mut _, std::mem::size_of::<T>() as u32)?;
    }
    Ok(data)
}

pub fn query_features(gpu: &Gpu, facts: &mut Facts) {
    match feature::<D3D12_FEATURE_DATA_GPU_VIRTUAL_ADDRESS_SUPPORT>(gpu, D3D12_FEATURE_GPU_VIRTUAL_ADDRESS_SUPPORT) {
        Ok(v) => {
            facts.va_bits_per_resource = v.MaxGPUVirtualAddressBitsPerResource;
            facts.va_bits_per_process = v.MaxGPUVirtualAddressBitsPerProcess;
        }
        Err(e) => facts.notes.push(format!("GPU virtual address query failed ({e})")),
    }
    match feature::<D3D12_FEATURE_DATA_D3D12_OPTIONS16>(gpu, D3D12_FEATURE_D3D12_OPTIONS16) {
        Ok(o) => {
            facts.options16_known = true;
            facts.gpu_upload_heap_supported = o.GPUUploadHeapSupported.as_bool();
        }
        Err(e) => facts.notes.push(format!("D3D12_OPTIONS16 query failed ({e}); the runtime may predate it")),
    }
}

pub fn submit(gpu: &Gpu, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> windows::core::Result<()> {
    // SAFETY: the allocator and list outlive the submission because we wait on the fence
    // before returning; the list is closed before it is executed.
    unsafe {
        let alloc: ID3D12CommandAllocator = gpu.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let list: ID3D12GraphicsCommandList =
            gpu.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None::<&ID3D12PipelineState>)?;
        record(&list);
        list.Close()?;
        gpu.queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>()?)]);
        let fence: ID3D12Fence = gpu.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        gpu.queue.Signal(&fence, 1)?;
        while fence.GetCompletedValue() < 1 {
            gpu.device.GetDeviceRemovedReason()?;
            std::thread::yield_now();
        }
        gpu.device.GetDeviceRemovedReason()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(name: &str, mib: u64, software: bool) -> AdapterInfo {
        AdapterInfo { name: name.into(), dedicated_video_memory: mib << 20, software }
    }

    #[test]
    fn picks_largest_hardware_adapter() {
        let list = [a("iGPU", 512, false), a("RX 7900 XTX", 24_560, false), a("Basic Render", 0, true)];
        assert_eq!(pick_adapter(&list), Some(1));
    }

    #[test]
    fn never_picks_software_adapter() {
        assert_eq!(pick_adapter(&[a("Basic Render", 99_999, true)]), None);
    }

    #[test]
    fn formats_driver_version_words() {
        let raw: i64 = (32_i64 << 48) | (21_025 << 16) | 1_010;
        assert_eq!(format_umd_version(raw), "32.0.21025.1010");
    }

    #[test]
    #[ignore = "needs the target machine"]
    fn opens_the_target_gpu() {
        let gpu = open(false).unwrap();
        assert!(gpu.info.name.contains("7900"), "picked {}", gpu.info.name);
        let mut facts = crate::report::Facts::default();
        query_features(&gpu, &mut facts);
        assert!(facts.va_bits_per_resource >= 32, "notes: {:?}", facts.notes);
        submit(&gpu, |_| {}).unwrap();
    }
}
