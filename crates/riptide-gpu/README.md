# riptide-gpu

The title's D3D12.X objects on host D3D12: command translation, promotion to video memory, texture conversion and background pipeline compilation.

## Interface

Implements the D3D12.X device, queue and command-list interfaces `riptide-hle` hands to the title. Each call on a title command list becomes host calls at once, on the title's thread. Buffers are used in place from title memory; large unchanging ones are promoted to video memory, and textures are converted there by a compute pass.

## Depends on

- `riptide-mem`
- `riptide-shader`
- `riptide-formats`
- `riptide-trace`

## Status

Not started. Piece 6 of the build order.
