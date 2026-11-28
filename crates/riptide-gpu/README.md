# riptide-gpu

The title's D3D12.X objects on host D3D12: command translation, promotion to video memory, texture conversion and background pipeline compilation.

## Interface

- `init(Services)` gives the crate what it needs from the runtime, once: the title's memory and allocator, where log lines go, and a maker of stubs for unbuilt methods.
- `serialize_versioned_root_signature` is the console's `D3D12SerializeVersionedRootSignature`.
- `create_device` is the console's `D3D12XboxCreateDevice`. It makes the host D3D12 device on the fastest graphics card the first time, and hands the title the one console device.

A console graphics object starts with 16 bytes a title's own code reads (table, bookkeeping bits, reference count), and its table has the console interface's full length. A method that is not built names itself (`ID3D12Device::CreateFence (slot 36, +0x120)`) and stops the title when called.

Planned: each call on a title command list becomes host calls at once, on the title's thread (decision D6). Buffers are used in place from title memory; large unchanging ones are promoted to video memory, and textures are converted there by a compute pass.

Setting `RIPTIDE_ADAPTER=software` makes the host device on Windows' software adapter. It is for tests on machines without a graphics card.

## Depends on

- `riptide-mem`
- `riptide-shader`
- `riptide-formats`
- `riptide-trace`

## Status

Started. The real title creates its device (on the RX 7900 XTX, from a 0x40-byte parameter block with interface version 2.4, as `ID3D12Device11`) and sets up its renderer as far as its first textures. It stops at `xg_xs.dll!XGSuggestSwizzleMode`, the console's texture-layout library, which is not built.

Built, and run under the real title:

- **Device.** Creation, `GetNodeCount`, `GetCustomHeapProperties`, and the device's DXGI side: DXGI device, adapter, factory and one output with one mode, 3840 by 2160 at 60 Hz.
- **Queues and fences.** `CreateCommandQueue`, `CreateCommandQueueX`, `Signal`, `Wait`; fences with `GetCompletedValue`, `SetEventOnCompletion`, `Signal`, `GetCreationFlags`. Each is a host object of the same kind.
- **Allocators and lists.** `CreateCommandAllocator`, `CreateCommandList`, `CreateCommandListX`, allocator `Reset`, list `GetType`, `Close`, `Reset`, and the two pipeline syncs, which become a wait for unordered-access writes. A list keeps the console's layout with its put pointer and limits at zero, so the title's code calls every command instead of writing packets.
- **Frame events.** `SetFrameIntervalX`, `ScheduleFrameEventX`, `WaitFrameEventX`, from a clock. The title asks for a frame every 33,333 us.
- **Descriptor heaps.** A heap is title memory: the title's own if its description goes on with an address and the right size, else memory committed for it. A handle is an address. Views take 32 bytes, samplers 16, render and depth targets 48.
- **Views and samplers.** Constants, shader resource, unordered access, render target and depth target views, the placed forms of the first two, `CreateSampler`, `CreateSamplerX`, and both descriptor copies. A view is a record that says everything about itself in its own bytes, so a title that copies descriptors as bytes copies views.
- **Resources.** `CreateResourceAtAddressX`, `CreateCommittedResource`, `CreateCommittedResource3`, `GetResourceAllocationInfo` and its newer form, and the resource's `Map`, `Unmap`, `GetDesc`, `GetGPUVirtualAddress`, `GetHeapProperties`. A resource records where in title memory it lies, and its GPU address is that address. A committed resource's memory comes from the title's own allocator when it has one, as on the console.
- **Smaller objects.** Query heaps (host objects), command signatures (a host signature at once when the arguments are only draws and dispatches), root signatures (packed and unpacked by the host's own functions), page pools.
- **Pipelines.** `CreatePipelineState` keeps the stream, the programs and the root signature. No host pipeline is made: the programs are the console GPU's code and `riptide-shader` cannot translate them yet, so nothing would be drawn with them.

Known to be wrong or missing:

- A texture is sized as its levels packed one after another, not by the console's layouts. The title's streamed textures need the console's sizes.
- No host object is made for a resource yet. Nothing has needed one, because no list method that uses a resource is built.
- Every recording method of a list except the two syncs, submission (`ExecuteCommandLists`), presenting, reserved resources and page mappings.
- An unordered access view's counter is not kept. The four words after a placed view's address are not understood.
- Frame events follow a clock, not the screen.

No unit test makes a host object; the host side has only run under the real title. Piece 6 of the build order.
