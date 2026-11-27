# riptide-probe

Measures whether the target machine supports Riptide's in-place memory design, before anything depends on it.

## Interface

`riptide-probe` prints a JSON report: whether a 14 GiB block of RAM opens as one D3D12 heap with one buffer, how fast the GPU reads it compared with video memory, whether shaders can write to it, what a write-protect fault costs, and CPU bandwidth to GPU upload heap memory. Exit code 0 means every requirement holds.

## Depends on

- `riptide-trace`

## Status

Not started. Piece 1 of the build order. Its results decide the open questions in `docs/design/2026-11-27-architecture.md`.
