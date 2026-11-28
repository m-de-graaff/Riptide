# Measurements

Every performance claim in this repository points to an entry here. Logs and captures stay in `local/`, which is never committed.

## Target machine

Radeon RX 7900 XTX (24 GB), Ryzen 7 7800X3D, 32 GB DDR5, Windows 11, Smart Access Memory on. Record the driver version with every entry.

## Entry format

Each entry gives: date, commit, driver version, scene and how it was reached, settings, warm or cold caches, session length, and the results (median frame time, 1% low, Riptide CPU time per frame, private memory, video memory, frames over 100 ms).

## Entries

### 2026-11-27 · riptide-probe · commit 8892833

Driver 32.0.32029.2010. `riptide-probe` release build. Windows 11 build 26200, 31.2 GiB of RAM visible, 16 GiB free when run.

| Check | Result |
| --- | --- |
| 14 GiB section as one in-place heap (Q1) | failed: `OpenExistingHeapFromFileMapping` gives "The parameter is incorrect" (0x80070057). 1 GiB passed |
| 14 GiB as seven 2 GiB in-place heaps, all open at once (Q1) | passed, data crossed both ways in each |
| Address bits per resource (Q1) | 47 (48 per process) |
| GPU copy, in-place to video (Q2) | 12.7 GiB/s |
| GPU copy, video to video (Q2) | 104.9 GiB/s |
| GPU copy, video to in-place (Q2) | 12.4 GiB/s |
| Scattered shader read, in-place (Q2) | 9.39 ns |
| Scattered shader read, video (Q2) | 0.155 ns |
| Shader reads in place are correct (Q2) | yes |
| Shader write to in-place memory (Q3) | yes |
| Write fault (Q4) | 2.45 us; protecting 1 GiB takes 20,979 us |
| GPU write passes a guard (Q4) | yes |
| CPU write / read, cached memory (Q5) | 24.6 / 47.8 GiB/s |
| CPU write / read, GPU upload memory (Q5) | 10.6 / 0.0152 GiB/s (the read is 15.6 MiB/s) |

Where the one-heap limit lies was found with a one-off test on the same day, same driver, not kept in the probe: a section opens as a heap at 1, 2, 3 GiB and at 4 GiB less 64 KiB (0xFFFF0000 bytes), and is refused at 4 GiB and every size above it. The refusal comes from opening the heap, before any buffer is placed. Four heaps of 0xFFFF0000 bytes (16 GiB in all) did not open together: the fourth failed with "Not enough memory resources" (0x8007000E), on a machine with 16 GiB of RAM free at the time.

### 2026-11-27 · riptide-hle timers and cores, riptide-mem in place · commit 47fcedc

Driver 32.0.32029.2010. Debug test builds, as `cargo test` makes them.

| Check | Result |
| --- | --- |
| 1 ms sleep, 1000 calls (`cargo test -p riptide-hle -- --nocapture`) | mean 1.512 ms, median 1.518 ms, worst 2.226 ms |
| 1 ms sleep called by the stand-in title, 1000 calls | mean 1.507 ms |
| Core layout (`cargo test -p riptide-hle --lib cores -- --nocapture`) | 8 physical cores; title mask 0x1555, Riptide mask 0xeaaa |
| GPU and CPU see each other's writes in `riptide-mem`'s block (`cargo test -p riptide-mem --test gpu -- --ignored`) | passed |

The 1 ms sleep is under the 2 ms the tests ask for, and half a millisecond over the 1 ms the console gives.

### 2026-11-27 · committed memory left behind by in-place heaps · commit 47fcedc

Driver 32.0.32029.2010, Windows 11 build 26200.

A section that has been opened as a D3D12 heap stays charged against the commit limit after the process that made it has exited. Seen after the probe runs above: Windows reported 102.2 GiB committed of a 105.0 GiB limit, while the private memory of every running process summed to 14.3 GiB. The figure did not fall over the following minutes. The page file had grown to 73.8 GiB to carry it.

Checked on a small scale: two runs of `cargo test -p riptide-mem --test gpu -- --ignored`, which opens one 64 MiB block as a heap and exits, raised committed memory by 82 MiB and then 56 MiB.

A reboot gives it back: 13.2 GiB committed afterwards, and the page file shrank again (commit limit 56.2 GiB).

Not yet known: whether restarting the graphics driver gives the memory back without a reboot, whether the driver or Windows holds it, and whether sections the heap was never opened on are affected (the 14 GiB section whose heap open failed appears not to be: the total fits the heaps that did open).

### 2026-11-27 · two ways to open memory as an in-place heap · commit 343fc10

Driver 32.0.32029.2010, Windows 11 build 26200. One-off tests in `riptide-probe`, release build, each run in its own process; committed memory read from Windows before the run and a few seconds after it exited.

| Heap opened | Works | CPU write through a second view of the section seen by the GPU | Committed memory left after four runs of 256 MiB |
| --- | --- | --- | --- |
| `OpenExistingHeapFromFileMapping` on a section | yes | yes | 1,026 MiB |
| `OpenExistingHeapFromAddress` on a view of a section | yes | yes | none (the figure fell by 23 MiB) |
| `OpenExistingHeapFromAddress` on `VirtualAlloc` memory | yes | not applicable | none in one run (the figure fell by 65 MiB) |

With the file-mapping heap, a view of part of the section mapped at its own address after the heap was opened was also seen by the GPU. `OpenExistingHeapFromAddress` on 6 GiB of `VirtualAlloc` memory was refused with "The parameter is incorrect", as the file-mapping heap is at 4 GiB and above.

Single runs move committed memory by up to about 60 MiB either way from other programs, which is why the first two rows are four runs each.

Spread between runs of the probe: three runs of the probe that day gave 102 to 146 GiB/s for the video-to-video copy and 7,000 to 21,300 us for protecting 1 GiB. The other figures moved by less than 10 %.

### 2026-11-27 · banks opened as heaps leave nothing committed · commit 5228dad

Driver 32.0.32029.2010, Windows 11 build 26200. `cargo test -p riptide-mem --test gpu -- --ignored`, debug build: each run makes title memory, opens a 2 GiB bank as a heap from its address, checks that the GPU and the CPU see each other's writes at the places the page table gives, and exits. Committed memory read from Windows before the first run and five seconds after the last.

| Runs | Passed | Committed before | Committed after | Change |
| --- | --- | --- | --- | --- |
| 4 | 4 | 24,990 MiB | 25,004 MiB | +14 MiB |

A leak of the kind measured for file-mapping heaps would have been 8 GiB. One run of the real title on the same memory (about 4.7 GiB committed by the title in banks, none opened as a heap yet) moved the figure by under 100 MiB, inside what other programs move it by.

### 2026-11-28 · the real title's shader programs, and its first two minutes · commit 41f71e8

Driver 32.0.32029.2010, Windows 11 build 26200. `riptide run` on the real title, debug build, in a 1280 by 720 window, stopped after a fixed time. Figures are read from the session log. One run each; they move a little from run to run with which thread gets where first.

Shader programs. Every distinct program the title made a pipeline with in the first minute was read and decoded by `riptide-shader`.

| Programs | Could not be read | Compute, pixel, vertex | Code words | Instructions | Not decoded | With a branch out of the code | Distinct instructions |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1,000 | 0 | 268, 505, 227 | 822,160 | 561,519 | 0 | 0 | 339 |

The count is reported at 1, 10, 100 and 1,000 programs; the title had made more than 1,000 by the end of the run, and the next figure is at 10,000. A program is a DXBC container whose `DXIL` part holds 4 bytes where its bitcode would be. The most used instructions were `v_mul_legacy_f32`, `v_fmac_f32`, `s_waitcnt`, `v_mov_b32` and `v_fma_f32`.

Two minutes of running, without the host's own checking.

| Ran for | Stopped by | Frames presented | Draws and dispatches recorded and skipped | Host errors |
| --- | --- | --- | --- | --- |
| 120 s | the time limit | more than 2,048 | more than 32,768 | none |

With the host checking every call (`RIPTIDE_GPU_CHECKS=1`), 60 s gave more than 1,024 frames and one message, that a render target was made without a clear value. Before commit 41f71e8 the host device was lost within the first minute: the title submits one recorded list every frame without resetting it, and the second submission ran a host list that had been reused.

Not measured: frame times, and what any of this costs in CPU time. Nothing is drawn yet, so neither would say much.

### 2026-11-28 · translating the real title's shader programs, and its first picture · commit f6ff6b5

Driver 32.0.32029.2010, Windows 11 build 26200. DirectX shader compiler 1.8.2502, from the Windows SDK 10.0.26100.0.

How many programs translate. `cargo run --release -p riptide-shader --example coverage -- <folder> --compile` over the 8,634 distinct programs the title made pipelines with in a longer run (written out with `RIPTIDE_SHADER_DUMP`), compiling on 16 threads.

| Stage | Programs | Lifted | Written as HLSL | Compiled |
| --- | --- | --- | --- | --- |
| Vertex | 1,034 | 1,017 | 1,017 | 1,017 |
| Pixel | 3,297 | 2,851 | 2,851 | 2,851 |
| Compute | 2,734 | 811 | 800 | 800 |
| Mesh | 1,569 | 0 | 0 | 0 |
| All | 8,634 | 4,679 | 4,668 | 4,668 |

Compiling the 4,668 took 5 min 44 s of wall time, about 1.2 s of one core for each on average. No translation that was written failed to compile, and none had control flow that could not be structured. What stops the rest is the lift: mesh programs, and instructions it does not have yet.

The first picture. `riptide run` on the real title, debug build, in a 1280 by 720 window, with the host checking every call (`RIPTIDE_GPU_CHECKS=1`), stopped after 180 s. One run.

| Ran for | Frames presented | Pipelines drawn with | Of them with a host pipeline | First picture other than black | Host messages |
| --- | --- | --- | --- | --- | --- |
| 180 s | more than 4,096 | 7 | 7 | about 85 s in | one, known: a render target made without a clear value |

The seven are six drawing pipelines and one compute pipeline. Before the picture the title's frames are black by its own doing: it clears, draws a full-screen rectangle multiplied by constants that are zero, and copies the result to the frame. Making a host pipeline took 3 to 6 ms with its shaders already kept on disc from an earlier run, about 90 ms for the two drawing pipelines met with nothing kept, and 557 ms for the compute pipeline. From about 85 s the frame is the title's first screen, with its row of button prompts, and stays so: the title waits there for a button.

Not measured: frame times, which the debug build and the host's checking would both distort.

### 2026-11-28 · past the first screen, with scripted input · commit e6b68ff

Driver 32.0.32029.2010, Windows 11 build 26200. `riptide run` on the real title, debug build, 1280 by 720 window, 570 s, with `RIPTIDE_INPUT_SCRIPT` pressing A at 100 s and then every 20 s from 240 s, and BCPack tables supplied through `RIPTIDE_BCPACK_TABLES`. One run, without the host's checking.

| Ran for | Frames presented | Host pipelines made | Screens reached | BCPack streams that did not decode | Stopped by |
| --- | --- | --- | --- | --- | --- |
| 570 s | more than 8,192 | 16 or more | first screen, navigation, brightness, legal notices, legal terms | 0 | the time limit |

The title reaches its page of legal terms, which asks the player to agree to them; the script stops there. Between the first press and the next screen the title takes about 140 s, most of it opening its packs. Its own check at one place in its pack reader (a breakpoint) was stepped over 16 times.

The BCPack decoder against the reference decoder, on 129 streams the title asked for in such a run (7 of BC1, 100 of BC5, 22 of BC7, written out with `RIPTIDE_BCPACK_DUMP`): all 129 come out the same byte for byte. Speed on one core, release build: about 70 MB of blocks a second for BC1, 65 for BC5 and 32 for BC7.

A run of 420 s with one press, with the host checking every call (`RIPTIDE_GPU_CHECKS=1`) lost no device. It says three things: the known one about a clear value, and two about the state of a bank's buffer (see `riptide-gpu`'s README).
