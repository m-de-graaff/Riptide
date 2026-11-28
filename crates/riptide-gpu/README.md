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

The real title creates its device (on the RX 7900 XTX, as `ID3D12Device11` with interface version 2.4), sets up its renderer, and runs: in two minutes it presents more than 2,000 frames into a window and reaches no call that is not built, with no error from the host. **The frames hold only what clears put there.** Nothing is drawn, because no host pipeline exists: a title's shader programs are the console GPU's code. `riptide-shader` reads and decodes them (every one of the title's first 1,000) and cannot translate them yet.

Built, and run under the real title:

- **Device.** Creation, `GetNodeCount`, `GetCustomHeapProperties`, and the device's DXGI side: DXGI device, adapter, factory and one output with one mode, 3840 by 2160 at 60 Hz.
- **Queues.** `CreateCommandQueue`, `CreateCommandQueueX`, `ExecuteCommandLists`, `ExecuteCommandListsX`, `Signal`, `Signal2X`, `Wait`, `GetTimestampFrequency`, `GetClockCalibration`, `PresentX` and `CopyPageMappingsBatchX`. Lists go to the host queue at once. Page mappings map the same frames of title memory at the destination.
- **Submission in order.** A GPU wait on a value in memory cuts a list in two, and the queue makes the wait between the parts on a thread of its own, with signals and presents taking their turn behind it. A list can be submitted again as it is, which the title does every frame; the queue waits for its run before to be over. With no wait and nothing queued, a submission still goes to the host at once on the title's thread.
- **Fences.** A fence lies in title memory with its value where a title reads it, and gives that address as its GPU address. A watcher thread carries the host fence's progress into the value and then sets the events waiting on it.
- **Allocators and lists.** A list keeps the console's layout with its put pointer and limits at zero, so the title's code calls every command. Recorded on the host list at once: viewports, scissors, topology, blend factor, stencil reference, depth bounds, shading rate, render targets and both clears, and immediate GPU writes into title memory. A barrier and a pipeline sync become a wait for shader writes. A list puts every host texture it used back in the common state when it is closed.
- **Presenting.** The presented texture is copied into a swap chain of its size in a window and shown as soon as it is ready, with tearing allowed. The window covers the screen without a border; `RIPTIDE_WINDOW=1280x720` makes it an ordinary window. Closing it ends the title.
- **Frame events.** `SetFrameIntervalX`, `ScheduleFrameEventX`, `WaitFrameEventX`, `GetFrameStatisticsX`, from a clock. The title asks for a frame every 33,333 us.
- **Title memory for the GPU.** Each bank is opened as a host heap with one buffer the first time a command needs it.
- **Descriptor heaps, views and samplers.** A heap is title memory and a handle is an address. A view is a record that says everything about itself in its own bytes, so a title that copies descriptors as bytes copies views. Render and depth target records become host views when a command names them.
- **Resources.** At title addresses, committed (memory from the title's own allocator, as on the console), and their sizes, which are the console layouts' own. A texture gets a host texture in video memory the first time a command needs one.
- **Smaller objects.** Query heaps, command signatures, root signatures (packed and unpacked by the host's own functions), page pools.
- **Pipelines.** Stream and compute pipelines keep their description, programs and root signature. No host pipeline is made. Every distinct program is read and decoded, and the session log counts what did not decode.
- **Checking.** `RIPTIDE_GPU_CHECKS=1` has the host check every graphics call and writes what it finds to the session log. A lost host device is always said there.

Known to be wrong or missing:

- **Nothing is drawn.** Draws and dispatches are counted and skipped, and the state only a pipeline can use (root arguments, descriptor heaps, vertex and index buffers) is accepted and dropped.
- State set on a list before a GPU wait is not carried over to the part of the list after it.
- Host textures hold only what is rendered into them: filling one from title memory (texture conversion) is not built. Copies, queries and indirect commands on a list are not built.
- A GPU timestamp write is given the time of recording, not of running. An unordered access view's counter is not kept. Frame events follow a clock, not the screen. No high dynamic range, one plane only, and the window does not follow the launcher's display settings.

No unit test makes a host object; the host side has only run under the real title. Piece 6 of the build order.
