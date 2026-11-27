# riptide-shader

Console shader code (RDNA 2) to DXIL, including decoding resource descriptors inside the shader.

## Interface

Translate one shader from its bytes to a DXIL blob plus a description of what it reads, keyed by content hash. Keeps the console's wave size. No knowledge of D3D12 objects.

## Depends on

Nothing in this workspace.

## Status

Not started. Piece 5 of the build order.
