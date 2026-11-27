//! The GPU reads bytes the CPU wrote to title memory, and the CPU reads bytes the GPU wrote
//! (decision D1). Needs a GPU, so it is ignored by default:
//!
//!     cargo test -p riptide-mem --test gpu -- --ignored

use riptide_mem::{PAGE, TitleMemory};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::{IUnknown, Interface, Result};

const MIB: u64 = 1 << 20;

struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
}

fn open() -> Result<Gpu> {
    // SAFETY: plain COM calls with valid out-pointers.
    unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None::<&IUnknown>, D3D_FEATURE_LEVEL_11_0, &mut device)?;
        let device = device.expect("D3D12CreateDevice succeeded without a device");
        let queue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        Ok(Gpu { device, queue })
    }
}

/// Records one command list, runs it and waits for it.
fn submit(gpu: &Gpu, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> Result<()> {
    // SAFETY: the allocator and the list outlive the submission, which is waited for.
    unsafe {
        let alloc: ID3D12CommandAllocator = gpu
            .device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let list: ID3D12GraphicsCommandList = gpu.device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &alloc,
            None::<&ID3D12PipelineState>,
        )?;
        record(&list);
        list.Close()?;
        gpu.queue
            .ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>()?)]);
        let fence: ID3D12Fence = gpu.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        gpu.queue.Signal(&fence, 1)?;
        while fence.GetCompletedValue() < 1 {
            gpu.device.GetDeviceRemovedReason()?;
            std::thread::yield_now();
        }
        gpu.device.GetDeviceRemovedReason()
    }
}

/// A mapped staging buffer of `bytes` in an upload or a readback heap.
fn staging(gpu: &Gpu, heap: D3D12_HEAP_TYPE, bytes: u64) -> Result<(ID3D12Resource, *mut u8)> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };
    let state = if heap == D3D12_HEAP_TYPE_UPLOAD {
        D3D12_RESOURCE_STATE_GENERIC_READ
    } else {
        D3D12_RESOURCE_STATE_COPY_DEST
    };
    let props = D3D12_HEAP_PROPERTIES {
        Type: heap,
        ..Default::default()
    };
    let mut resource: Option<ID3D12Resource> = None;
    let mut p = std::ptr::null_mut();
    // SAFETY: valid descriptions and out-pointers; a buffer has one subresource.
    unsafe {
        gpu.device.CreateCommittedResource(
            &props,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut resource,
        )?;
        let resource = resource.expect("CreateCommittedResource succeeded without a resource");
        resource.Map(0, None, Some(&mut p))?;
        Ok((resource, p.cast()))
    }
}

fn pattern(salt: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8 ^ salt)
        .collect()
}

#[test]
#[ignore = "needs a D3D12 GPU"]
fn the_gpu_and_the_cpu_see_each_others_writes_at_the_converted_offset() {
    let gpu = open().unwrap();
    let mem = TitleMemory::create(64 * MIB).unwrap();
    let map = mem.address_map();
    let heap = mem
        .open_heap(&gpu.device, D3D12_RESOURCE_FLAG_NONE)
        .unwrap();
    let len = 16 * PAGE;

    // Two allocations away from the start of the block, so a wrong offset shows.
    mem.reserve(None, 3 * MIB).unwrap();
    let written_by_cpu = mem.allocate(len).unwrap();
    let written_by_gpu = mem.allocate(len).unwrap();
    assert_ne!(map.offset(written_by_cpu), 0);

    // The CPU writes through the title's address; the GPU copies out from the offset.
    let from_cpu = pattern(0x11, len as usize);
    // SAFETY: `len` committed bytes at an address this test owns.
    unsafe { std::slice::from_raw_parts_mut(written_by_cpu as *mut u8, len as usize) }
        .copy_from_slice(&from_cpu);
    let (readback, p) = staging(&gpu, D3D12_HEAP_TYPE_READBACK, len).unwrap();
    submit(&gpu, |list| {
        // SAFETY: both ranges lie inside their buffers.
        unsafe {
            list.CopyBufferRegion(&readback, 0, &heap.buffer, map.offset(written_by_cpu), len)
        }
    })
    .unwrap();
    // SAFETY: `readback` maps `len` bytes, all written by the finished copy.
    let seen_by_gpu = unsafe { std::slice::from_raw_parts(p, len as usize) }.to_vec();
    assert!(
        seen_by_gpu == from_cpu,
        "the GPU did not read what the CPU wrote"
    );

    // The GPU copies in at the offset; the CPU reads through the title's address.
    let from_gpu = pattern(0x22, len as usize);
    let (upload, p) = staging(&gpu, D3D12_HEAP_TYPE_UPLOAD, len).unwrap();
    // SAFETY: `upload` maps `len` writable bytes.
    unsafe { std::ptr::copy_nonoverlapping(from_gpu.as_ptr(), p, len as usize) };
    submit(&gpu, |list| {
        // SAFETY: both ranges lie inside their buffers.
        unsafe { list.CopyBufferRegion(&heap.buffer, map.offset(written_by_gpu), &upload, 0, len) }
    })
    .unwrap();
    // SAFETY: `len` committed bytes at an address this test owns.
    let seen_by_cpu =
        unsafe { std::slice::from_raw_parts(written_by_gpu as *const u8, len as usize) };
    assert!(
        seen_by_cpu == from_gpu.as_slice(),
        "the CPU did not read what the GPU wrote"
    );

    // The neighbouring allocation is untouched by the GPU's write.
    // SAFETY: as above.
    let neighbour =
        unsafe { std::slice::from_raw_parts(written_by_cpu as *const u8, len as usize) };
    assert!(
        neighbour == from_cpu.as_slice(),
        "the GPU's write landed in the wrong place"
    );
}
