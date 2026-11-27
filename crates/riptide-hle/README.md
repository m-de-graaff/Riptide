# riptide-hle

Console API implementations, and mapping title threads onto host cores.

## Interface

Exports the functions a title imports from console DLLs. Unimplemented functions fail with their name. Title threads get one physical core each on the 7800X3D, matching the console's core numbers.

## Depends on

- `riptide-pe`
- `riptide-mem`
- `riptide-trace`

## Status

Not started. Piece 4 of the build order, together with `riptide-pe`.
