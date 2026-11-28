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

`riptide_formats::bcpack` unpacks BCPack, the console's lossless packing of block-compressed textures, which its storage hardware undoes while reading:

- `decode(format, packed, blocks, &tables)` turns the packed bytes of one read into BC blocks. `format` is DirectStorage's number, 1 to 7 for BC1 to BC7. `blocks` must be exactly the decoded size, because packed data does not say how long it is.
- `Tables::parse(bytes)` reads the constants of the format (entropy models, rank tables, BC7 control words, dictionaries) from a file of `TABLES_BYTES` bytes. The constants are built into the console's hardware and are not in this repository: the user supplies the file.
- `Error` says in plain words what was wrong with the input or the tables.

## Depends on

Nothing in this workspace.

## Status

Texture layouts are built (issue #32). BCPack decoding is built (issue #33). XMA2 is not started.

What the texture layouts cover:

- Sizes, alignments, mip offsets and mip tails for every mode: row-major, and the 256-byte, 4 KiB and 64 KiB families in their standard, display, render and depth variants, with and without XOR. Elements of 1, 2, 4, 8 and 16 bytes, block compression, arrays, cubes, volumes, and multisampling with 2, 4 or 8 samples.
- Element offsets and copies for row-major and the six modes without XOR (256 B, 4 KiB and 64 KiB, standard and display), single-sampled. The tests check these against sizes and per-element addresses produced by AMD's address library.

What they do not cover:

- Element offsets and copies for the XOR modes (every `X` and `T` variant, which includes all render and depth modes) and for multisampled textures. Those addresses depend on the GPU's pipe and bank configuration, which is not known here. The functions return `Error::Unsupported`; sizes are still computed.
- Compression metadata planes of render targets and depth buffers. Such textures come out smaller than on the console.
- The 64 KiB tile map of a thick volume.
- Formats of 96 bits, 1 bit, packed 4:2:2 and video formats.

Two things are assumptions rather than checked facts, and their doc comments say so: the suggested-mode rule comes from an older function of the console's library than the one titles call, and one-dimensional textures in a tiled mode are laid out as two-dimensional textures one element high.

What BCPack covers:

- Decoding of BC1, BC3, BC4, BC5, BC6H and BC7, all modes of the last two, with references into earlier blocks and into the dictionaries.
- Malformed input: truncated data, a BC6H mode or BC7 control code that does not exist, an output that is not a whole number of blocks and a tables file of the wrong length or with broken models are errors. No input makes the decoder panic or touch memory outside the slices it was given. Most other damage cannot be noticed, because the format has no checksums, and decodes to wrong blocks.
- The tests run on made-up tables and on streams from a packer that exists only in the tests. They show that the decoder inverts that packer; a few blocks are checked against bytes worked out by hand.

What it does not cover:

- BC2. Its packed form is not known; `decode` returns `Error::Bc2`.
- Packing. There is no encoder outside the tests.
- The tables file itself. Without one nothing can be decoded.

The format was learnt from the BCPack decoder of the VIctory project, the only description of it there is. Agreement with that decoder was checked outside this repository, on chunks a title requested and with the real tables; neither can be committed, so that check is not part of the test suite.
