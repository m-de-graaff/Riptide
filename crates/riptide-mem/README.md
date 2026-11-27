# riptide-mem

The title's memory: one committed block of RAM that is also a D3D12 heap, its allocator, write guards, and console-to-host address conversion.

## Interface

Reserve and commit ranges the way the console's memory APIs do; convert a console GPU address to an offset in the one buffer (one subtraction); write-protect a range and report the first write to it.

## Depends on

- `riptide-trace`

## Status

Not started. Piece 3 of the build order.
