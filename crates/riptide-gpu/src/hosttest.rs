//! A test of what a translated shader is bound to, on a real GPU: the page table, the
//! banks of title memory in the shaders' heap, a block of stage inputs and the one root
//! signature, with shaders written by hand to the same rules a translation follows.
//!
//! It needs a graphics card and the shader compiler, so it is run by name:
//! `cargo test -p riptide-gpu -- --ignored bindings`.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use riptide_mem::{Access, FRAME, Limits, TitleMemory};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

use crate::hostpipe::Shaders;
use crate::pipeline::Subobject;
use windows::core::Interface;

/// What every shader here starts with: the stage inputs, the page table, and reading a
/// word of title memory by its address. `SPACE` is the space of the stage's inputs.
const START: &str = r"
cbuffer Inputs : register(b0, SPACE) { uint4 input_words[20]; }
ByteAddressBuffer page_table : register(t0, space0);

uint user(uint n) { return input_words[n >> 2][n & 3]; }

// The word of title memory at a 64-bit address given as two halves. Zero where nothing is
// mapped, or beyond what the page table covers.
uint read_word(uint low, uint high) {
    if (high >= 32) return 0;
    uint entry = page_table.Load(((low >> 16) | (high << 16)) * 4);
    if (entry == 0) return 0;
    uint frame = entry - 1;
    ByteAddressBuffer bank = ResourceDescriptorHeap[frame >> 15];
    return bank.Load(((frame & 0x7fff) << 16) | (low & 0xfffc));
}
";

const VERTEX: &str = r"
float4 main(uint id : SV_VertexID) : SV_Position {
    return float4(id == 2 ? 3.0 : -1.0, id == 1 ? 3.0 : -1.0, 0.0, 1.0);
}
";

const PIXEL: &str = r"
float4 main() : SV_Target {
    uint word = read_word(user(0), user(1));
    return float4(word & 0xff, (word >> 8) & 0xff, (word >> 16) & 0xff, word >> 24) / 255.0;
}
";

fn unbuilt(name: String) -> usize {
    panic!("{name} is not built");
}

/// A stream of a root signature, a vertex and a pixel program, depth and stencil turned
/// off, triangles, and one target of format 28.
fn stream() -> (Vec<u8>, Vec<Subobject>) {
    let mut stream = Vec::new();
    let mut subobjects = Vec::new();
    let mut add = |kind: u32, payload: &[u8]| {
        let offset = stream.len();
        stream.extend_from_slice(&kind.to_le_bytes());
        stream.extend_from_slice(payload);
        while stream.len() % 8 != 0 {
            stream.push(0);
        }
        subobjects.push(Subobject {
            kind,
            offset,
            bytes: stream.len() - offset,
        });
    };
    add(0, &[0; 12]);
    add(1, &[0; 20]);
    add(2, &[0; 20]);
    add(11, &[0; 52]);
    add(14, &3u32.to_le_bytes());
    let mut formats = [0u8; 36];
    formats[..4].copy_from_slice(&28u32.to_le_bytes());
    formats[32..].copy_from_slice(&1u32.to_le_bytes());
    add(15, &formats);
    (stream, subobjects)
}

/// A transition of `resource` from one state to another.
fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

#[test]
#[ignore = "needs a graphics card and the shader compiler"]
fn a_shaders_bindings_reach_title_memory_through_the_page_table() {
    let memory: &'static TitleMemory = Box::leak(Box::new(TitleMemory::new(4 << 30)));
    crate::init(crate::Services {
        memory,
        log: Arc::new(|line| eprintln!("{line}")),
        stub: unbuilt,
        allocator: || None,
    });
    let host = crate::host::host().expect("a host device");
    assert!(crate::compile::found_in().is_some(), "no shader compiler");

    // A word of title memory, above 4 GiB so that both halves of its address count.
    let limits = Limits {
        lowest: 4 << 30,
        highest: (1 << 37) - 1,
        alignment: 0,
    };
    let at = memory
        .commit(None, FRAME, limits, Access::ReadWrite)
        .unwrap()
        + 0x1230;
    // SAFETY: committed just now, and inside the frame.
    unsafe { (at as *mut u32).write(0x80ff_4020) };
    crate::pagetable::sync();
    crate::bindless::sync();

    let vertex = crate::compile::compile(
        &format!("{}{VERTEX}", START.replace("SPACE", "space0")),
        "main",
        "vs_6_6",
        &[],
    )
    .unwrap();
    let pixel = crate::compile::compile(
        &format!("{}{PIXEL}", START.replace("SPACE", "space1")),
        "main",
        "ps_6_6",
        &[],
    )
    .unwrap();
    let (stream, subobjects) = stream();
    let pipeline = crate::hostpipe::drawing(
        &stream,
        &subobjects,
        &[],
        &Shaders {
            vertex: Some(vertex),
            pixel: Some(pixel),
            geometry: None,
        },
    )
    .unwrap();

    let mut inputs = [0u32; crate::stage::WORDS];
    (inputs[0], inputs[1]) = (at as u32, (at >> 32) as u32);
    let block = crate::stage::block(1, &inputs).expect("a block of stage inputs");

    // SAFETY: plain calls on the host device and objects made here, in the order the host
    // wants them; the read-back buffer is read inside its size after the GPU is done.
    unsafe {
        let device = &host.device;
        let target_description = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        };
        let mut target: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &target_description,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut target,
            )
            .unwrap();
        let target = target.unwrap();
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut bytes = 0u64;
        device.GetCopyableFootprints(
            &target_description,
            0,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut bytes),
        );
        let mut read_back: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
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
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut read_back,
            )
            .unwrap();
        let read_back = read_back.unwrap();

        let views: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .unwrap();
        let view = views.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&target, None, view);

        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())
            .unwrap();
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();

        list.SetDescriptorHeaps(&crate::bindless::heaps().expect("the shaders' heaps"));
        list.SetGraphicsRootSignature(&crate::stage::root_signature().unwrap());
        list.SetGraphicsRootShaderResourceView(
            crate::stage::PAGE_TABLE,
            crate::pagetable::buffer().unwrap().GetGPUVirtualAddress(),
        );
        list.SetGraphicsRootConstantBufferView(crate::stage::INPUTS + 1, block);
        list.SetPipelineState(&pipeline);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: 4.0,
            Height: 4.0,
            MaxDepth: 1.0,
            ..Default::default()
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        }]);
        list.OMSetRenderTargets(1, Some(&view), false, None);
        list.ClearRenderTargetView(view, &[0.0; 4], None);
        list.DrawInstanced(3, 1, 0, 0);
        list.ResourceBarrier(&[transition(
            &target,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyTextureRegion(
            &D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(read_back.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint,
                },
            },
            0,
            0,
            0,
            &D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(target.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            },
            None,
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        fence.SetEventOnCompletion(1, event).unwrap();
        WaitForSingleObject(event, INFINITE);
        crate::host::report_problems();
        device.GetDeviceRemovedReason().unwrap();

        let mut mapped = std::ptr::null_mut();
        read_back.Map(0, None, Some(&mut mapped)).unwrap();
        let pixels = std::slice::from_raw_parts(mapped.cast::<u8>(), bytes as usize);
        // Every pixel is the word of title memory, a byte to a channel.
        let last = 3 * footprint.Footprint.RowPitch as usize + 12;
        assert_eq!(pixels[..4], [0x20, 0x40, 0xff, 0x80]);
        assert_eq!(pixels[last..last + 4], [0x20, 0x40, 0xff, 0x80]);
    }
    crate::stage::give_back(1);
}
