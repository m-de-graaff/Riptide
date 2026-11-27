# riptide-probe

Measures whether the target machine supports Riptide's in-place memory design, before anything depends on it.

## Interface

`riptide-probe` prints a JSON report: whether a 14 GiB block of RAM opens as one D3D12 heap with one buffer, how fast the GPU reads it compared with video memory, whether shaders can write to it, what a write-protect fault costs, and CPU bandwidth to GPU upload heap memory. Exit code 0 means every requirement holds.

## Depends on

Nothing in this workspace. (It will use `riptide-trace` once that exists.)

## Running it

    cargo build -p riptide-probe --release
    target/release/riptide-probe.exe > local/probe/report.json

Add `--debug` to turn on the D3D12 debug layer (needs Windows' Graphics Tools optional
feature); its messages explain a refused call.

Exit code 0: every required check passed. 1: a required check failed. 2: the probe could not
open the GPU at all. The probe reserves 14 GiB against the page file for a moment.

## Status

Built; not yet run on the target machine. Piece 1 of the build order. Its results decide the open questions in `docs/design/2026-11-27-architecture.md`.
