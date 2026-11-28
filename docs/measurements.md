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
