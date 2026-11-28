# riptide-formats

Texture tiling and the console's compressed texture and audio formats.

## Interface

Pure functions from console bytes to host bytes. No GPU or OS dependencies and no unsafe code, so every function is testable on synthetic input.

`riptide_formats::texture` is the arithmetic of the console GPU's texture layouts ("swizzle modes"):

- `Mode` names the sixteen layout modes and converts to and from their hardware numbers. `suggested_mode` gives the mode the console's library picks for a texture that names none.
- `Texture::new(&TextureDesc)` lays out a whole texture from a DXGI format number: one plane, or depth and stencil as two. `PlaneLayout::new(&PlaneDesc)` lays out a single plane of any element size.
- A `PlaneLayout` gives the total bytes and alignment, the block dimensions, whether the mode is thick, and for each `MipLevel` its offset, pitch, padded rows and depth, size in elements and texels, slice size, and its place in the mip tail.
- `element_offset` gives the byte offset of one element.
- `tile` and `detile` copy one subresource between plain rows (with a caller-given row pitch and depth pitch, `LinearPitch`) and the console layout.
- `tile_map` gives the division of a two-dimensional 64 KiB texture into 64 KiB tiles, as the console's tiled-resource query reports it.

## Depends on

Nothing in this workspace.

## Status

Texture layouts are built (issue #32). BCPack and XMA2 are not started.

What the texture layouts cover:

- Sizes, alignments, mip offsets and mip tails for every mode: row-major, and the 256-byte, 4 KiB and 64 KiB families in their standard, display, render and depth variants, with and without XOR. Elements of 1, 2, 4, 8 and 16 bytes, block compression, arrays, cubes, volumes, and multisampling with 2, 4 or 8 samples.
- Element offsets and copies for row-major and the six modes without XOR (256 B, 4 KiB and 64 KiB, standard and display), single-sampled. The tests check these against sizes and per-element addresses produced by AMD's address library.

What they do not cover:

- Element offsets and copies for the XOR modes (every `X` and `T` variant, which includes all render and depth modes) and for multisampled textures. Those addresses depend on the GPU's pipe and bank configuration, which is not known here. The functions return `Error::Unsupported`; sizes are still computed.
- Compression metadata planes of render targets and depth buffers. Such textures come out smaller than on the console.
- The 64 KiB tile map of a thick volume.
- Formats of 96 bits, 1 bit, packed 4:2:2 and video formats.

Two things are assumptions rather than checked facts, and their doc comments say so: the suggested-mode rule comes from an older function of the console's library than the one titles call, and one-dimensional textures in a tiled mode are laid out as two-dimensional textures one element high.
