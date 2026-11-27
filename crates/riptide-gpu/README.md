# riptide-gpu

The title's D3D12.X objects on host D3D12: command translation, promotion to video memory, texture conversion and background pipeline compilation.

## Interface

- `init(Services)` gives the crate what it needs from the runtime, once: the title's memory, where log lines go, and a maker of stubs for unbuilt methods.
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

Started. The real title creates its device (on the RX 7900 XTX, from a 0x40-byte parameter block with interface version 2.4, as `ID3D12Device11`), asks for its heap rules, makes its command queues and fences, and stops at `CreateCommandAllocator`.

Built: device creation, `GetNodeCount`, `GetCustomHeapProperties`, command queues (`CreateCommandQueue`, `CreateCommandQueueX`, `Signal`, `Wait`) and fences (`GetCompletedValue`, `SetEventOnCompletion`, `Signal`, `GetCreationFlags`), each on a host object of the same kind.

Not built: command allocators and lists, submission, resources, descriptors, pipelines and presenting. No unit test makes a host object yet; the host side has only run under the real title. Piece 6 of the build order.
