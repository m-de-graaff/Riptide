# Riptide architecture

Approved 2026-11-27, revised the same day (see "What changed"). The README explains the reasoning and the target machine. This file records the decisions, the build order and what is still unknown, so they are not reopened by accident.

## Decisions

Q1's answer (2026-11-27): one heap cannot hold the block on the target machine. D1 and D2 below still describe one heap and one subtraction; they take the several-heaps form Q1 names, which is not designed yet.

- **D1. Title memory is one block of RAM that the GPU uses in place.** Riptide owns the title's allocator. The block is one committed section, opened as a D3D12 heap (`OpenExistingHeapFromFileMapping`) with one buffer over it. The GPU reads and writes the title's bytes directly; the CPU works at RAM speed. There is no mirror on this path.
- **D2. One address conversion.** A console GPU address becomes an offset in that buffer with one subtraction, in shaders and on the CPU.
- **D3. Promotion is an optimisation, never a requirement.** Data that is large, used every frame and unwritten for a while gets a copy in video memory, and its RAM pages are write-protected. A write fault drops the copy and the protection before the write continues. With every promotion off, the picture must be the same.
- **D4. Textures are converted once, on the GPU.** A compute pass reads the console layout in place and writes a host texture. Source pages are write-protected like promoted data; a write marks the texture for conversion at its next use. GPU-written textures the CPU reads are written back to title memory by a compute pass.
- **D5. Descriptors are decoded in the shader.** Translated shaders read the title's raw descriptors from title memory. Textures resolve through a GPU-side hash table into one bindless descriptor heap. The CPU does no per-draw descriptor work.
- **D6. Commands are translated as recorded.** Each title call on a command list becomes host calls on a host list, on the title's thread. No translation threads, no command cache, no work at submission. A cache is added only if a measurement asks for it.
- **D7. No frame waits for the shader compiler.** Shader code is translated ahead of time in a prepare step. Known pipelines compile at launch on all 16 hardware threads. New ones compile in the background, and their draws are skipped until ready (switchable to waiting).
- **D8. The title's CPU code runs natively.** One physical core per title thread, matching the console's core numbers. Riptide's own work has a one-core budget.
- **D9. Smart Access Memory is optional.** GPU upload heaps are used only for data the CPU writes and never reads.
- **D10. Hard memory budgets.** A RAM budget and a video-memory budget are set at launch from what the machine has free (video memory from DXGI's budget query). Near a limit, promoted copies and converted textures unused for longest are dropped first; D3 makes that safe. Pages the title frees are returned to Windows at once. RAM accounting lives in `riptide-mem`, video-memory accounting in `riptide-gpu`.
- **D11. Preflight and a live cause readout.** `riptide` checks free RAM, page file and the title's drive before starting, and refuses with the reason if they cannot carry the title. `riptide-trace` attributes every frame to paging (hard page faults), shader compiling, Riptide's CPU work, the title's CPU work or the GPU, and the launcher shows it.
- **D12. Fast file path.** The title's file reads are carried unbuffered and asynchronously into title memory, with no intermediate copy. The title decides what to stream; Riptide only carries the reads. Lives in `riptide-hle`.
- **D13. Frame generation is the driver's job.** AMD Fluid Motion Frames is the documented way to show more than the title's 30 rendered frames. Riptide implements none of its own.

### Why D10 to D13

The user ran all five existing runtimes on the target machine: 30 fps in menus, 4 to 10 in the open world, 27 at best. Two causes fit and were not told apart: memory overflow (those runtimes use 40 to 46 GB; the machine has 32) and per-draw CPU cost on eight cores. D1 and D6 address the causes. D10 keeps the first from returning, D11 makes either visible at once, and the 10,000-draw bar on piece 6 catches the second before a real title runs.

## Smaller decisions

Each is small, belongs to one crate, and is designed in detail with that crate's piece.

| # | Decision | Crate |
| --- | --- | --- |
| S1 | **1 ms timers.** The title's sleeps and timed waits use high-resolution waitable timers, not Windows' default 15.6 ms tick. | `riptide-hle` |
| S2 | **Present when ready.** A flip-model swap chain with tearing allowed, so a FreeSync display follows the title's frame rate. | `riptide-gpu` |
| S3 | **HDR passthrough.** A title's HDR10 output goes to an HDR display unchanged; it is tone-mapped only for an SDR display. | `riptide-gpu` |
| S4 | **Compressed textures stay compressed.** Conversion (D4) changes a texture's layout and never unpacks block-compressed data. | `riptide-gpu`, `riptide-formats` |
| S5 | **Device loss is recoverable.** Title memory in RAM is the only truth, so after a driver reset every host object, promoted copy and converted texture is rebuilt and the title carries on. | `riptide-gpu` |
| S6 | **Atomic saves.** A save is written to a temporary file and renamed into place. | `riptide-hle` |
| S7 | **Picture-equality tests need no GPU.** The "promotion on and off give the same picture" check (D3) runs on the software adapter (WARP), on any machine and in CI. | `riptide-gpu` tests |
| S8 | **Pipeline library.** The driver's compiled pipelines are stored in a D3D12 pipeline library keyed by adapter and driver version. Launch loads them, and compiles again only after a driver update. | `riptide-gpu` |
| S9 | **Learned promotion.** What stayed unwritten last session is recorded (addresses and sizes of title resources by creation order, no game data) and promoted at once in the next. A wrong guess costs one fault (D3). | `riptide-gpu` |
| S10 | **Preflight warnings.** D11's check also warns about a power-saving plan, real-time antivirus scanning of the cache folder, and a GPU driver older than the one last measured. Warnings do not block starting. | `riptide` |

## What changed on 2026-11-27

The first version of D1 put title memory in video memory, written by the CPU through Smart Access Memory. It was dropped after reading the other projects' source and measurements:

- Kelvaroq measured CPU reads from GPU upload heap memory at about 30 MB/s. Titles read their own graphics memory (decompressing streamed data reads back its output), so pointing title pointers there would stall loading and streaming.
- The host GPU cannot sample console texture layouts, so "no second copy of anything" could not hold for textures.
- The command cache assumed titles record the same lists every frame. An open-world title does not.

Vice-Tay's `VICTORY_INPLACE` experiment showed the RAM-side alternative works on a driver: a section opened as a heap is not locked into memory, views of it can be remapped and protected while the heap lives, a buffer in it needs `ALLOW_CROSS_ADAPTER`, and tile mappings into such a heap remove the device. Riptide uses no tile mappings.

## Build order

Each piece ships on its own and gets its own design and plan.

| # | Crate | Done when |
| --- | --- | --- |
| 0 | `riptide-launcher`, `riptide-config`, `riptide-preflight`, `riptide-log` | Done 2026-11-27. Design: `2026-11-27-launcher.md` |
| 1 | `riptide-probe` | It answers Q1 to Q5 below on the target machine |
| 2 | `riptide-trace` | Other crates can record stages and a session log can be summarised |
| 3 | `riptide-mem` | The stand-in title's memory calls work, guards fault and recover, and address conversion is one subtraction |
| 4 | `riptide-pe`, `riptide-hle` | The stand-in title loads, binds its imports and runs to its exit |
| 5 | `riptide-shader` | Synthetic RDNA 2 programs translate and match a reference on the GPU |
| 6 | `riptide-gpu` | A stand-in title draws from in-place memory, with promotion on and off giving the same picture, and a synthetic 10,000-draw frame costs under 5 ms of Riptide CPU time on the target machine |
| 7 | `riptide-formats` | Each format round-trips synthetic input |
| 8 | `riptide` | A title starts from the command line |

## Open questions

Numbered so plans and measurements can refer to them. The probe answers Q1 to Q5.

- **Q1.** Does a 14 GiB committed section open as one heap with one buffer on the Adrenalin driver, and do CPU writes and GPU writes each show up on the other side? If not at that size, D1 uses several heaps and D2 becomes a table lookup. If not at all, D1 fails and the memory design is redone before piece 2.
  **Answer (2026-11-27):** Not at that size. A section of 4 GiB or more does not open as a heap on driver 32.0.32029.2010; the largest that opens is 4 GiB less 64 KiB. Seven 2 GiB sections do open as seven heaps at once, with CPU and GPU writes each seen on the other side in all of them. So D1 holds in its several-heaps form and D2 becomes a table lookup. `riptide-mem` still builds one section and one heap, and has to change before a block of 4 GiB or more is used. [Measurements](../measurements.md#2026-11-27--riptide-probe--commit-8892833).
- **Q2.** How fast does the GPU read in-place memory compared with video memory, for bulk copies and for scattered small reads? This sets how much D3 has to promote, and whether D5's in-shader descriptor reads can stay in place.
  **Answer (2026-11-27):** Bulk copies run at 12.7 GiB/s from in-place memory to video memory and 12.4 GiB/s back, against 104.9 GiB/s inside video memory: about 8 times slower. A scattered 4-byte shader read costs 9.39 ns in place and 0.155 ns in video memory: about 60 times slower. The sums read in place were correct. Scattered reads are the case for promotion; whether D5's descriptor reads can stay in place is decided with these figures when `riptide-gpu` is designed. [Measurements](../measurements.md#2026-11-27--riptide-probe--commit-8892833).
- **Q3.** Can a shader write to the in-place buffer (unordered access)? If not, GPU writes to title memory go through copies.
  **Answer (2026-11-27):** Yes. A compute shader wrote to the in-place buffer through unordered access and the CPU read what it wrote. [Measurements](../measurements.md#2026-11-27--riptide-probe--commit-8892833).
- **Q4.** What does a write fault on a protected page cost, and what does protecting a range cost? This sets how D3 and D4 group pages.
  **Answer (2026-11-27):** A write fault costs 2.45 us from the write to the write going on. Protecting 1 GiB took 7 to 21 ms across runs, which is 0.03 to 0.08 us per 4 KiB page. A GPU write to a protected page lands without a fault, so guards never hear of GPU writes. [Measurements](../measurements.md#2026-11-27--riptide-probe--commit-8892833).
- **Q5.** With Smart Access Memory on, what are CPU write and read bandwidth to GPU upload heap memory? This confirms or corrects the 30 MB/s read figure behind D1 and D9.
  **Answer (2026-11-27):** The CPU writes GPU upload memory at 10.6 GiB/s and reads it at 15.6 MiB/s, against 24.6 and 47.8 GiB/s for ordinary memory. The read figure is confirmed and is lower than assumed: D9's rule stands, the CPU never reads that memory. [Measurements](../measurements.md#2026-11-27--riptide-probe--commit-8892833).
- **Q6.** Shaders addressing more than 4 GiB: does the driver accept a 4-byte-word view over 14 GiB, or are windows needed? Measured in piece 5.
- **Q7.** Does Riptide's one-core budget hold when the title keeps seven cores busy, and does using the second hardware thread of title cores slow the title? Measured in piece 4.
- **Q8.** AMD-specific shader faults reported by VIctory-RDNA (helper lanes in quad wave reads, compiler crashes on some pipeline flags) need tests in piece 5.
- **Q10.** Does AMD Fluid Motion Frames engage on Riptide's swap chain, and which presentation mode does it need? Checked in piece 8.
- **Q11.** Which of the two causes in "Why D10 to D13" made the existing runtimes slow on the target machine? Answered by watching memory use during a session of one of them; it sets how tight D10's default budgets must be.
- **Q12.** Do 2 MiB large pages for title memory work with D1's section-backed heap and D3's write guards, and how much do they cut the title's CPU time? Large pages cannot be paged out and are protected 2 MiB at a time, so this is measured in piece 3 and adopted only if both answers are good.
- **Q9.** Promotion thresholds: how large, how often used and how long unwritten before data is promoted. Set from Q2 and Q4, tuned in piece 6.
