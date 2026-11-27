//! The GPU reads bytes the CPU wrote to title memory, and the CPU reads bytes the GPU wrote,
//! in place (decisions M1 and M3). Needs a GPU, so it is ignored by default:
//!
//!     cargo test -p riptide-mem --test gpu -- --ignored

use riptide_mem::{Access, BANK_BYTES, FRAME, Limits, TitleMemory};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::{IUnknown, Interface, Result};

const ANYWHERE: Limits = Limits {
    lowest: 0x1_0000_0000,
    highest: (1 << 40) - 1,
    alignment: 0,
};

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
    // SAFETY: valid descriptors and out-pointers; the buffer stays mapped while it lives.
    unsafe {
        let mut buffer: Option<ID3D12Resource> = None;
        gpu.device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: heap,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut buffer,
        )?;
        let buffer = buffer.expect("CreateCommittedResource succeeded without a resource");
        let mut mapped = std::ptr::null_mut();
        buffer.Map(0, None, Some(&mut mapped))?;
        Ok((buffer, mapped.cast()))
    }
}

fn pattern(salt: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt))
        .collect()
}

#[test]
#[ignore = "needs a GPU"]
fn the_gpu_and_the_cpu_see_each_others_writes_where_the_page_table_says() {
    const BYTES: usize = 64 * 1024;
    let gpu = open().unwrap();
    let memory = TitleMemory::new(BANK_BYTES);

    // Physical pages mapped at two addresses: the CPU writes through one of them.
    let range = memory.reserve(64 * FRAME, ANYWHERE).unwrap();
    let frames = memory.allocate_frames(4).unwrap();
    let (first, second) = (range + 4 * FRAME, range + 40 * FRAME);
    memory
        .map_frames(first, &frames, Access::ReadWrite)
        .unwrap();
    memory
        .map_frames(second, &frames, Access::ReadWrite)
        .unwrap();
    // And committed memory, for the GPU to write into.
    let pool = memory
        .commit(None, 4 * FRAME, ANYWHERE, Access::ReadWrite)
        .unwrap();

    let heap = memory
        .open_heap(0, &gpu.device, D3D12_RESOURCE_FLAG_NONE)
        .unwrap();

    // CPU to GPU: written through the second address, found through the first.
    let from_cpu = pattern(0x11, BYTES);
    // SAFETY: `second + FRAME` is mapped read-write for at least `BYTES`.
    unsafe {
        std::ptr::copy_nonoverlapping(from_cpu.as_ptr(), (second + FRAME) as *mut u8, BYTES);
    }
    let place = memory.locate(first + FRAME, BYTES as u64).unwrap();
    assert_eq!(place.len(), 1, "one frame is one part of one bank");
    let (readback, read_at) = staging(&gpu, D3D12_HEAP_TYPE_READBACK, BYTES as u64).unwrap();
    submit(&gpu, |list| {
        // SAFETY: both buffers hold the ranges copied.
        unsafe { list.CopyBufferRegion(&readback, 0, &heap.buffer, place[0].offset, BYTES as u64) }
    })
    .unwrap();
    // SAFETY: the readback buffer is mapped for `BYTES`.
    let seen_by_gpu = unsafe { std::slice::from_raw_parts(read_at, BYTES) };
    assert!(
        seen_by_gpu == from_cpu,
        "the GPU did not see the CPU's bytes"
    );

    // GPU to CPU: written at the pool's place in its bank, read at its address.
    let from_gpu = pattern(0x22, BYTES);
    let (upload, write_at) = staging(&gpu, D3D12_HEAP_TYPE_UPLOAD, BYTES as u64).unwrap();
    // SAFETY: the upload buffer is mapped for `BYTES`.
    unsafe { std::ptr::copy_nonoverlapping(from_gpu.as_ptr(), write_at, BYTES) };
    let place = memory.locate(pool + 2 * FRAME, BYTES as u64).unwrap();
    submit(&gpu, |list| {
        // SAFETY: both buffers hold the ranges copied.
        unsafe { list.CopyBufferRegion(&heap.buffer, place[0].offset, &upload, 0, BYTES as u64) }
    })
    .unwrap();
    // SAFETY: `pool + 2 * FRAME` is mapped for at least `BYTES`.
    let seen_by_cpu = unsafe { std::slice::from_raw_parts((pool + 2 * FRAME) as *const u8, BYTES) };
    assert!(
        seen_by_cpu == from_gpu,
        "the CPU did not see the GPU's bytes"
    );

    // The heap goes before the memory it lies on.
    drop(heap);
    drop(memory);
}
