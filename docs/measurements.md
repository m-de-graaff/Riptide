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

Spread between runs: three runs of the probe that day gave 102 to 146 GiB/s for the video-to-video copy and 7,000 to 21,300 us for protecting 1 GiB. The other figures moved by less than 10 %.
