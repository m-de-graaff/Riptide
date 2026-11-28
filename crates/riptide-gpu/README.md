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

The real title creates its device (on the RX 7900 XTX, as `ID3D12Device11` with interface version 2.4), sets up its renderer, and runs: in two minutes it presents more than 2,000 frames into a window and reaches no call that is not built, with no error from the host. **It draws.** A title's shader programs are the console GPU's code; `riptide-shader` translates them to host shaders, and the title's first screen, its background and its row of button prompts, is drawn by them into the window, about a minute and a half after starting, with nothing said by the host's own checking. The title then waits for a button, and input is not built.

Built, and run under the real title:

- **Device.** Creation, `GetNodeCount`, `GetCustomHeapProperties`, and the device's DXGI side: DXGI device, adapter, factory and one output with one mode, 3840 by 2160 at 60 Hz.
- **Queues.** `CreateCommandQueue`, `CreateCommandQueueX`, `ExecuteCommandLists`, `ExecuteCommandListsX`, `Signal`, `Signal2X`, `Wait`, `GetTimestampFrequency`, `GetClockCalibration`, `PresentX` and `CopyPageMappingsBatchX`. Lists go to the host queue at once. Page mappings map the same frames of title memory at the destination.
- **Submission in order.** A GPU wait on a value in memory cuts a list in two, and the queue makes the wait between the parts on a thread of its own, with signals and presents taking their turn behind it. A list can be submitted again as it is, which the title does every frame; the queue waits for its run before to be over. With no wait and nothing queued, a submission still goes to the host at once on the title's thread.
- **Fences.** A fence lies in title memory with its value where a title reads it, and gives that address as its GPU address. A watcher thread carries the host fence's progress into the value and then sets the events waiting on it.
- **Allocators and lists.** A list keeps the console's layout with its put pointer and limits at zero, so the title's code calls every command. Recorded on the host list at once: viewports, scissors, topology, blend factor, stencil reference, depth bounds, shading rate, render targets and both clears, and immediate GPU writes into title memory. A barrier and a pipeline sync become a wait for shader writes. Host textures rest in a state every shader can read; a list that needs one in another state puts it there, and puts it back before a draw that is not drawing to it and when the list is closed. The pipeline, root signatures and arguments, descriptor heaps and vertex and index buffers a title sets are kept in the list for the draws.
- **Presenting.** The presented texture is copied into a swap chain of its size in a window and shown as soon as it is ready, with tearing allowed. The window covers the screen without a border; `RIPTIDE_WINDOW=1280x720` makes it an ordinary window. Closing it ends the title.
- **Frame events.** `SetFrameIntervalX`, `ScheduleFrameEventX`, `WaitFrameEventX`, `GetFrameStatisticsX`, from a clock. The title asks for a frame every 33,333 us.
- **Title memory for the GPU.** Each bank is opened as a host heap with one buffer the first time a command needs it. The GPU has a page table of its own, one word per 64 KiB of title addresses below 2^37, kept current from `riptide-mem` before lists are submitted. Title memory is not handed out above 2^37.
- **Descriptor heaps, views and samplers.** A heap is title memory and a handle is an address. A view is a record that says everything about itself in its own bytes, so a title that copies descriptors as bytes copies views. A record has the console GPU's own descriptor in the words compiled shader code reads (a buffer's address, element size and count; a texture's size, levels and kind), and the place of the texture's host view, or of the host sampler, in two host heaps every translated shader reads from. Those heaps also hold each bank of title memory, to read and to write. Heaps committed here lie below 4 GiB, since a table reaches a shader as a 32-bit address. Render and depth target records become host views when a command names them.
- **Resources.** At title addresses, committed (memory from the title's own allocator, as on the console), and their sizes, which are the console layouts' own. A texture gets a host texture in video memory the first time a command or a view needs one; depth textures are made without a type so that shaders can read them. A texture a view for shaders was made of is kept filled from title memory: converted out of the console's layout on the CPU before the next lists run, and again when a guard on its memory says the title wrote it.
- **Smaller objects.** Query heaps, command signatures, root signatures (packed and unpacked by the host's own functions), page pools.
- **Pipelines.** Stream and compute pipelines keep their description, programs, root signature and vertex layout. The host pipeline is made the first time something is drawn with one, on threads kept for that, and what is drawn with it meanwhile is not drawn: each program translated and compiled (the DirectX shader compiler is looked for beside `riptide.exe`, in `RIPTIDE_DXC`, in a Windows SDK and in a Vulkan SDK), the one host root signature, and the title's own fixed state. Every distinct program is read and decoded, and the session log counts what did not decode. A pipeline that draws rectangles gets a geometry shader that completes each from its three vertices, and leaves out neither side of them. `RIPTIDE_SHADER_DUMP` names a folder to write each program and each host shader's source to, for work on the translator.
- **Draws.** `DrawInstanced`, `DrawIndexedInstanced` and `Dispatch` are made where the pipeline has a host pipeline: each stage's inputs (the console's user data, from the title's root arguments) are written to a block the GPU reads, and vertex and index buffers are bound where they lie in title memory.
- **Checking.** `RIPTIDE_GPU_CHECKS=1` has the host check every graphics call and writes what it finds to the session log: each different thing the first time, and again each time its count doubles. `RIPTIDE_DRAW_LOG=<count>` writes the first draws there in full: their programs, what the list had set, each stage's inputs and the title memory those lead to. A lost host device is always said there.

Known to be wrong or missing:

- **Not every program translates.** A pipeline with a program `riptide-shader` cannot translate has no host pipeline, and what is drawn with it is counted and not made: mesh programs, most compute programs and about one pixel program in seven (see `riptide-shader`'s README for the counts). Indirect draws, mesh draws and the console's own dispatch are not built. Quads and line loops are not completed as rectangles are.
- **What a translated shader reads is checked on few programs.** The title's first seven pipelines draw what they should. Images are sampled with the approximations `riptide-shader`'s README lists, and a pixel shader that reads more rows than its vertex shader writes has not been met.
- Depth bounds and the shading rate set on a list before a GPU wait are not carried over to the part of the list after it; the rest is.
- Texture conversion runs on the CPU, on the thread that submits. A copy from one buffer to another on a list, or between two addresses with the console's `CopyMemoryX`, is made by the host GPU where the bytes lie in title memory, by way of a scratch buffer when both are in one bank; copies of textures and queries on a list are not built. A vertex or index buffer that does not lie in one stretch of one bank cannot be bound.
- The host's checking says a bank's buffer is in the wrong state when one list writes it (an immediate write, a copy) and another of the same submission binds vertices or indices from it, although each list records the change of state the host's rules ask for. Nothing is drawn wrong by it on this GPU. A depth texture the title also wants to write from a compute program is made as a depth target only, and has no view for writing.
- A texture whose memory a copy on a list fills is not filled again on the host: only what the title's CPU writes is noticed. Textures the title streams in and then copies into place may therefore show as they were before.
- A GPU timestamp write is given the time of recording, not of running. An unordered access view's counter is not kept. Frame events follow a clock, not the screen. No high dynamic range, one plane only, and the window does not follow the launcher's display settings.

One test makes host objects, on a real GPU, and is run by name (`cargo test -p riptide-gpu -- --ignored bindings`): shaders written by hand to the rules a translation follows read title memory through the page table. The rest of the host side has only run under the real title. Piece 6 of the build order.
