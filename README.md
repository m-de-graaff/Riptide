# Riptide

Riptide is an Xbox Series X|S compatibility runtime for Windows, written in Rust. It runs a title's x86-64 code natively on the host CPU, provides the console APIs the title calls, and runs its graphics work on Direct3D 12. You supply the title from your own console.

It is built for one machine first: a Radeon RX 7900 XTX, a Ryzen 7 7800X3D and 32 GB of DDR5. Every design choice below is made for that hardware. Other machines come later, once this one reaches its targets.

> **Status: design stage.** Only the launcher exists; nothing runs a title yet. This README describes the architecture Riptide is being built toward and the measurements it has to hit. Every claim below is a target until a benchmark in `docs/measurements.md` says otherwise.

## Why another runtime

The working runtimes today, VIctory and its forks (Vice-Tay, VIctory-RDNA, Kelvaroq, AnyXboxX), all share one design. On the console, the CPU and GPU share one pool of memory. On a PC they don't, so those runtimes keep two copies of everything and spend the frame keeping them in sync. Their own findings put it plainly: every large cost is noticing the title's writes, noticing the shaders' writes, moving the data, or waiting until an order is safe.

Their published numbers on an RX 7900 XTX show where that leads:

- In a 55 ms frame, 48 ms is spent on the one thread that prepares GPU submissions. The title itself needs 33 ms.
- 36,000 command-list calls per frame are translated on the CPU, and 67,000 in a street chase.
- 400,000 descriptor slots are scanned and 40,000 copied every frame.
- 40 to 46 GB of private memory is used. That does not fit in 32 GB at all.
- Minutes of each session are lost to frames over 200 ms while new pipelines compile.

Those projects have made each of these costs narrower, wider or later. In all of them, a missed write means a wrong picture: their notes list sunk boats, lost writes and device losses that came from a copy going stale.

Riptide turns the design around. The GPU works on the title's memory where it lies, so the picture is right without noticing anything. Copies are an optional speed-up for data that has proven it does not change.

## The target machine

| Part | What matters about it | What Riptide does with it |
| --- | --- | --- |
| 32 GB DDR5 | The console gives a title 13.5 GB in total. Windows and background programs take about 6 GB, which leaves roughly 26 GB. | All title memory lives in ordinary RAM, once. Riptide's whole process stays under 16 GB of private memory. |
| RX 7900 XTX, 24 GB | Enough video memory for every texture and every hot buffer of a 13.5 GB title, with room to spare. | Video memory holds converted textures and promoted buffers, and never has to be reclaimed mid-frame. |
| RX 7900 XTX, RDNA 3 | A direct descendant of the console's RDNA 2 GPU. It runs 64-wide waves natively, as console shaders expect. | Translated shaders keep the console's wave size instead of being rewritten for 32-wide waves. |
| PCIe 4.0 x16 | The path the GPU uses to read title memory in RAM. | Measured first, by `riptide-probe`. How much data needs promoting depends on it. |
| Ryzen 7 7800X3D, 8 cores | The console gives a title 7 cores. That leaves one core, plus the second hardware thread of each core, for Riptide. | Title threads get one physical core each, matching the console's core numbers. Riptide's own work has a budget of one core. |
| Ryzen 7 7800X3D, 96 MB L3, one CCD | Every core shares one large cache, with no cross-chiplet latency. | Riptide's lookup tables (address map, promotion table, pipeline cache) are sized to stay in L3. |

The CPU is the tight part of this machine, not the GPU. The existing runtimes need eight translation threads and a dedicated submission thread on top of the title's own seven. On eight cores, that work competes with the title. Riptide's design leaves almost nothing to do per frame beyond the title's own calls.

## The approach

### 1. The GPU works on title memory in place

Riptide owns the title's memory allocator. All title memory is one block of ordinary RAM, and the same block is opened as a Direct3D 12 heap with one buffer over it. The GPU reads and writes the title's bytes where they lie, and the title's CPU code reads and writes them at full RAM speed. A console GPU address converts to an offset in that buffer with a single subtraction, in the shaders and on the CPU.

Nothing is mirrored on this path, so nothing has to be noticed, moved or waited for. What a shader writes, the CPU sees at once. What the CPU writes, the next draw sees.

One of the existing runtimes has this as an experiment that is off by default. It is limited there by an allocator that was not built for it: a 4 GB cap, and a memory pool the GPU cannot open. Riptide's allocator is built around it from the start.

Pointing the title's pointers into video memory instead was considered and rejected. CPU reads from video memory were measured at about 30 MB/s, and titles read their own graphics memory all the time, for example while decompressing streamed data.

### 2. Data that has stopped changing is promoted to video memory

Reading from RAM over PCIe is slower for the GPU than reading its own memory. So data that is large, used every frame and has not been written for a while gets a copy in video memory. That is mostly vertex and index buffers. Its pages in RAM are then write-protected. The first write to them raises a fault; Riptide drops the copy, removes the protection and lets the write go on, and the data is used in place again.

Promotion can only make a frame faster. If Riptide promotes too little, frames are slower. If a write comes, the fault catches it. Neither produces a wrong picture.

### 3. Textures are converted once, on the GPU

The host GPU cannot sample the console's texture layout, so textures are the one thing that always needs a host copy. A compute pass on the GPU reads the title's bytes in place and writes the host texture. The source pages are then write-protected in the same way as promoted buffers, and a texture is converted again only after a write. Textures the GPU writes and the CPU later reads are written back into title memory by another compute pass.

### 4. Shaders read the console's descriptors where the console left them

A console title writes its resource descriptors as raw bytes into memory. Existing runtimes scan those bytes on the CPU and rebuild host descriptor tables every frame. In Riptide, translated shaders read the title's descriptors from title memory and decode them on the GPU. Buffer descriptors become an offset and a size in the one buffer. Texture descriptors are looked up in a GPU-side hash table that maps a console texture to a slot in one bindless host descriptor heap, filled when a texture is converted.

The CPU no longer touches descriptors per draw.

### 5. Commands are translated as the title records them

Each call the title makes on a command list becomes host calls on a host command list, at once, on the title's own thread. The existing runtimes do the same, and pay for it because every call also has to look after mirrors. With no mirrors, a call is a few table lookups. There are no translation threads and no extra step at submission.

### 6. No frame waits for the shader compiler

Console shader code is translated to host shaders ahead of time, in a prepare step before the title runs. Pipelines the title has created before are recorded as state descriptions (no game data) and compiled on all 16 hardware threads at launch. A pipeline seen for the first time compiles on a background thread, and until it is ready its draws are skipped for at most a few frames. Pop-in on first sight is the trade Riptide makes for never stalling; it can be turned off to wait instead.

### 7. Hard memory budgets

The existing runtimes use 40 to 46 GB in the open world. On a 32 GB machine that overflows into the page file, and the frame rate falls to single digits. Riptide sets a RAM budget and a video-memory budget at launch, from what the machine has free, and tracks its use against them all the time. Near a limit it drops video-memory copies, least recently used first. It can do that because those copies are never needed for a right picture; the data is still in title memory and is used in place. Pages the title frees go back to Windows at once.

### 8. A low frame rate always comes with a reason

Before the title starts, Riptide checks free RAM, the page file and the drive the title is on, and refuses to start if they cannot carry it, with the reason. While the title runs, a readout shows where each frame's time went: paging, shader compiling, Riptide's own CPU work, the title's CPU work, or the GPU. A slow frame is never a mystery.

### 9. The title's own streaming gets a fast path

The title streams its assets from its own files, as it does on the console. Riptide does not decide what is loaded, but it carries every read: file reads go to the drive unbuffered and asynchronously, straight into title memory, with no copy in between.

### Smaller things that add up

- **1 ms timers.** The title's short sleeps take 1 ms as on the console, not Windows' default 15.6 ms.
- **Present when ready.** Frames go to the display the moment they are done, so a FreeSync display follows the title's frame rate without judder.
- **HDR passthrough.** HDR10 output reaches an HDR display unchanged.
- **Compressed textures stay compressed.** Converting a texture never unpacks it, which keeps video memory low.
- **A driver reset is not the end of the session.** Title memory in RAM is the only truth, so after a GPU device loss Riptide rebuilds what was in video memory and the title carries on.
- **Saves cannot be half-written.** A save is written beside the old one and swapped in.
- **Launch loads pipelines instead of compiling them.** The driver's compiled pipelines are kept in a library and rebuilt only after a driver update.
- **Riptide remembers what was static.** Data that never changed last session is promoted to video memory at once in the next.
- **Preflight warnings** for a power-saving plan, antivirus scanning of the cache folder and an old GPU driver.

### What stays the same

The title's CPU code runs natively, as in every working runtime. The 7800X3D runs the console's Zen 2 code faster than the console does, so interpreting or recompiling it would gain nothing. Console APIs are implemented in Rust behind the title's imports, and host Windows functions are used directly where the console's version is the same.

## Targets

These are the measurements Riptide has to reach on the target machine before it is called working. Each is measured on a fixed scene with a published method.

| Measure | Target |
| --- | --- |
| Rendered frame rate, open world, 1440p | 30 fps median, the title's own cap |
| Rendered frame rate, open world, 4K | 30 fps median (the best published figure on RDNA 3 today is 23.1) |
| Frame pacing | 1% low within 10% of the median |
| Riptide's own CPU time per frame | Under 5 ms, on at most one core's worth of threads |
| Riptide's CPU time in a synthetic 10,000-draw frame | Under 5 ms. Checked before any real title runs |
| Private system memory | Under 16 GB for the whole process |
| Video memory | Under 22 GB, leaving headroom for the driver and the desktop |
| Frames over 100 ms after warm-up | None |

## More frames on screen

The title caps itself at 30 rendered frames a second. To show more, turn on AMD Fluid Motion Frames in the Adrenalin software. It works at driver level with any Direct3D 12 program, so Riptide needs no code for it, and it doubles the frames shown. It does not raise the rendered rate, so it helps once that rate is steady; below about 25 rendered frames a second it adds visible artefacts.

## Requirements

- Windows 11 x64
- A current AMD Adrenalin driver
- A page file with room for a 14 GB reservation on top of what is already in use
- A legally obtained Xbox Series title from your own console

Smart Access Memory (Resizable BAR) is optional. With it on, data the CPU only ever writes, such as per-frame constants, can go straight to video memory.

## Planned layout

One Cargo workspace:

| Crate | Role |
| --- | --- |
| `riptide-launcher` | the window where every setting is made, in place of flags and environment variables |
| `riptide-config` | the settings file (`%APPDATA%\Riptide\riptide.toml`): defaults, range checks, load and save |
| `riptide-preflight` | checks before starting that the machine can carry the title, with reasons |
| `riptide-log` | the session log in `%LOCALAPPDATA%\Riptide\logs`, written by the launcher and the runtime |
| `riptide-probe` | measures whether the machine supports the in-place design, before anything depends on it |
| `riptide-mem` | the title's memory block, its allocator, write guards and address conversion |
| `riptide-pe` | loading, mapping and binding the title image |
| `riptide-hle` | console API implementations, thread and core mapping |
| `riptide-gpu` | D3D12.X objects, command translation, promotion, texture conversion |
| `riptide-shader` | console shader code to DXIL, including in-shader descriptor decoding |
| `riptide-formats` | texture tiling, compressed texture and audio formats |
| `riptide-trace` | frame timing, per-stage cost ledger, replay of recorded command streams |
| `riptide` | command-line front end and launcher |

`cargo build-7800x3d` makes the release build for the target machine (`target-cpu=znver4`, with AVX-512, full link-time optimization). That build does not run on CPUs without Zen 4's instructions, so plain `cargo build` and `cargo test` stay portable.

The launcher, its settings library and the preflight checks exist and are tested. Start the launcher with `cargo run -p riptide-launcher`. Everything else is still a design.

## Rules

- **No game, console or SDK files in the repository, ever.** Tests use inputs built from our own source. `cargo xtask policy` checks every tracked file, and `cargo xtask hooks` makes git run the same check before each commit.
- **Measure before and after.** A performance change lands with the frame-time ledger from both runs.
- **No title-specific code in the core.** Fixes are made in the platform layers, so they apply to every title.
- **An unimplemented console function fails loudly**, with its name, rather than returning a guess.
- **Copies are never needed for a right picture.** Every promotion and conversion can be switched off, and the output must stay the same.

## Open questions

These are the parts of the design that could fail, listed so they get tested first:

- **GPU reads over PCIe may be too slow as the default.** If the GPU reads in-place buffers much more slowly than video memory, more data has to be promoted, and the design moves closer to what the existing runtimes do. This is the first thing `riptide-probe` measures.
- **One heap over the whole block.** Opening a 14 GiB block of RAM as one heap with one buffer has to work on the AMD driver. The existing experiment opened parts of 4 GB at most.
- **Write faults cost time.** Each first write to protected data costs a fault. Streaming writes many pages, so protection has to be dropped per resource, not per page, and the fault has to be cheap.
- **Shaders address more than 4 GiB.** A raw buffer offset in a shader is 32 bits. Addressing by 4-byte word covers 16 GiB; whether the driver accepts a view that large has to be tested.
- **Eight cores is little room.** If the title keeps all seven of its cores busy, Riptide's one-core budget has to hold under load. Riptide's threads running on the second hardware thread of the title's cores may slow the title down; that has to be measured.
- **Known RDNA 3 pitfalls.** The VIctory-RDNA findings report AMD-specific faults: helper lanes not taking part in quad wave reads, and the driver's compiler crashing on some pipeline flags. Riptide's shader tests have to cover both.

## Prior work

Riptide is written from scratch. The published findings and measurements of VIctory, Vice-Tay, VIctory-RDNA, Kelvaroq and AnyXboxX shaped the problem statement and ruled out an earlier version of this design. No code is taken from them, since most are under GPL-3.0.

## License

To be decided.
