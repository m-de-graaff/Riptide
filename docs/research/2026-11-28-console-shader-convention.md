# How console shader code gets its inputs and resources

Gathered 2026-11-28 for the shader translator (`docs/design/2026-11-28-shader-translation.md`). These are facts about the console's compiled shader code, restated from another project's reading of shipped titles; nothing here is that project's code. Line references are to its source as checked out under `.opensrc/` on the day, which is not part of this repository. Where it says "VIctory does", that is what that project does with the fact, recorded because it shows what the code needs, not as a design to follow.

Nothing in this file has been checked against the real title by Riptide yet, except where the translator's own notes say so.

Sources are under `D:\GTAVI\Riptide\.opensrc\VIctory-RDNA\crates\`. I write `vs/` for `victory-shader\src\` and `vg/` for `victory-gpu\src\`. Line numbers are approximate.

How far I read: `native.rs`, `io.rs` and the non-test half of `resource.rs` closely; every comment and signature of `lower.rs` (13,400 non-test lines), `structure.rs`, `ir.rs`, `wave.rs`, `spill.rs`, `emit/mod.rs`, `emit/shader.rs`, and the code of the functions that carry the facts below (`smem`, `descriptor_data`, `handle`, `heap_index`, `image`, `buffer`, `export`, `inputs`, `user`). On the GPU side I read comments of `descriptor.rs`, `pipeline.rs`, `pipeline/host.rs`, `draw.rs`, the head of `descriptor/host.rs`, and the code of root-signature creation and `bind`. I did not read `gfx10/` beyond the MIMG/MUBUF field decode, nor `bvh.rs`/`intersect.rs` (ray tracing).

Two things affect your design decisions T5–T7 directly; they are detailed in sections 1 and 3:

- **A table register is 32 bits.** The code gets only a 32-bit heap address and zeroes the high half itself.
- **Compiled code reads descriptor fields as numbers.** Image width/height/levels/depth, buffer record count, and sometimes a buffer's address are read from the loaded dwords, so your 32-byte record cannot be opaque unless the translator intercepts those reads (which is what VIctory does).

---

## 1. Root parameters to registers

**User-data registers** (`vs/native.rs` 156–165):

| Hardware stage | SGPR holding user-data dword N |
|---|---|
| VS, PS, CS | s(N) |
| LS, HS, ES, GS (merged stages), MS (mesh) | s(N+8): eight system SGPRs come first |

**`RootParam` (20 bytes)** has the same fields as your `RootParameter` (`vs/native.rs` 388–402, 694–701): root type, offset of the range in its table (dwords), count of user SGPRs, first shader register (b/t/u/s number), first user-data dword, root signature slot.

**Root type codes and register contents** (`vs/native.rs` 358–385; `vs/resource.rs` 1–13, 968–980):

| Code | Kind | SGPRs | What the register holds on entry |
|---|---|---|---|
| 0 | Root constants | one per dword | The constant values themselves |
| 1 | Root descriptor (CBV/SRV/UAV) | 2 | A 64-bit buffer address: low dword in the first SGPR, high dword in the next. Not a descriptor: the code builds one around it (section 3) |
| 3 | Descriptor table | 1 | The 32-bit heap address of the table; the code sets the high half to 0 itself (sometimes inside a branch, used after the join; `resource.rs` 938–946) |
| 4 | Immediate (static) sampler | 0 | Nothing; the four sampler words are constants in the code |
| 5 | Resource heap base (SM 6.6 dynamic resources) | 1 | 32-bit address of the CBV/SRV/UAV heap |
| 6 | Sampler heap base | 1 | 32-bit address of the sampler heap |

- **Table arithmetic.** The code adds offsets in console descriptor units, 32 bytes per view and 16 per sampler (`vs/emit/mod.rs` 9–16). VIctory recovers a heap index as (register + instruction offset + offset register + 4 × first dword) >> 5, or >> 4 for samplers (`vs/emit/lower.rs` 8354–8392).
- **What VIctory puts in the register.** The byte offset of the table's first descriptor from the heap start, with heap bases taken as 0 (`vg/draw.rs` 1463–1469; `lower.rs` 3222).
- **Not found:** whether several ranges of one table share one SGPR or get one each. The parser keeps one entry "per parameter or table range"; VIctory supplies the table start and matches used-resource entries by `table_offset_dwords <= dword read`, which suggests the register is the table start and the code adds the range offset. I did not see this stated outright.

**Spilled user data, compute only** (`vs/spill.rs` 1–17, 53):
- When the root signature needs more dwords than `COMPUTE_PGM_RSRC2` gives user SGPRs, no parameter is in a register.
- s0 and `s1 & 0xff` then hold a 40-bit address of a table in memory. The parameter the layout puts at user-data index N (N ≥ 16) is at byte 4 × (N − 16).
- Only this arrangement (table starting at index 16) was seen, in the ray-tracing structure builder's programs: 118 of 118 loads landed on a declared parameter.

**System registers by stage:**
- **Compute** (`vs/native.rs` 318–356). `COMPUTE_PGM_RSRC2` decides:
  - bits 1–5: user SGPR count;
  - bits 7, 8, 9: thread-group id x, y, z, each in the next SGPR after the user ones;
  - bit 10: the thread-group info SGPR after those;
  - bit 0: scratch enabled, adding one more initialised SGPR (the wave offset);
  - bits 11–12: thread-id components in v0..v2, minus 1;
  - bits 15–23: LDS size in 512-byte granules.
  - SGPRs above the initialised ones hold whatever the previous wave left; VIctory reads them as 0 (`lower.rs` 2897–2903).
- **Mesh** (`vs/io.rs` 25–32). s3 holds the wave's index in the group in bits 24–27; v5 of the first lane holds group id × group size.
- **Pixel with an ordered section.** One input SGPR is the POPS collision wave id: bit 31 means the wave overlaps another, bits 28–29 name the packer (`lower.rs` 2926–2950). VIctory reads it as 0.
- **Other merged stages** (LS/HS/ES/GS): the meaning of their eight system SGPRs is not found. I/O raising refuses those stages (`io.rs` 305), and their stage data "is kept in another layout (not read)" (`native.rs` 64).

## 2. The used-resource table

`UsedResource`, 20 bytes (`vs/native.rs` 413–426, 707–722):

| Field | Meaning |
|---|---|
| Resource type (u8) | 0 SRV, 1 sampler, 2 CBV, 3 UAV |
| Root type (u8) | Same codes as section 1: how the code reaches it |
| Table offset (dwords) | Where the range starts in its table (bytes = × 4) |
| Lower bound | Shader register of the first descriptor read |
| Stride | Element size in bytes for structured buffers (see below) |
| Descriptor dwords (u8) | 4 or 8 |
| User-data dword (u8) | Which user-data register the parameter is in |
| Root slot (u8) | Root parameter index |

- **Register space is not stored**; only the slot and register.
- **Stride.** VIctory uses it as the multiplier for indexed buffer accesses: a stride ≥ 4 that is a multiple of 4 is a structured element size. Typed and raw views have at most 4, and "a stride of 4 or less may be a typed view's component count" (`lower.rs` 3424–3427, 7007–7008, 12361–12375).
- **Finding the entry for a load.** The entry with the same root type and user-data dword and the greatest table offset not above the dword loaded (`lower.rs` 7846–7865).
- **Views picked from the heap by a computed index** (root type 5) have no entry. Their stride is not in the code either: the hardware takes it from the descriptor (`vs/emit/shader.rs` 16–25).
- **The FSPX form** (`ACCC` part, the FidelityFX upscaler's compiler) has 32-byte records with a view dimension in place of the stride: 1 buffer, 4 2D, 0 root view (`native.rs` 460–482).

## 3. Descriptors as code sees them

**Where the knowledge comes from.** VIctory never writes hardware descriptors. The title makes views through API calls, and VIctory's 32-byte heap record is a tag ("vVIE") plus two words naming its own view entry (`vg/descriptor.rs` 1127–1145). Titles also copy descriptors as raw bytes, 1,300 to 14,400 slots a frame, so records must survive byte copies (`vg/descriptor.rs` 362–369). Everything below is therefore what the compiled code was seen to read, not a full hardware layout.

**Sizes.** View descriptors are 32 bytes, sampler descriptors 16 (`vg/descriptor.rs` 4–7). An image is 8 dwords; a buffer, sampler or constant buffer uses 4. In `MIMG` the resource is 8 SGPRs, or 4 with the r128 bit (`vs/gfx10/fields.rs` 385).

**Image descriptor fields the code computes with** (`lower.rs` 3420–3423, 6932–6943, 7179–7214). The compiler does not query sizes; it extracts them:

| Field | Position |
|---|---|
| Width − 1 | Low 2 bits in dword 1 bits 30–31; the rest in dword 2 bits 0–13 |
| Height − 1 | Dword 2 bits 14–29 (one `s_bfe_u32`, offset 14, width 16) |
| Channel swizzle | Dword 3 bits 0–11 (identity is 0xFAC) |
| Base level | Dword 3; the exact bits are not stated |
| Last level | Dword 3 from bit 16 |
| Type | Dword 3 bits 28–31: 8 = 1D, 9 = 2D, 10 = 3D, 12 = 1D array, 13 = 2D array; cube and MSAA codes not given |
| Depth − 1 or layers − 1 | Dword 4 |

- Dword 0, the rest of dword 1 and dwords 5–7 (address, format, tiling) are only passed to the instruction. VIctory has no value for them, and a program that computes with them fails with "a descriptor's address or format read as data".
- Shortcut in the source: the base level reads 0.

**Buffer descriptor (4 dwords):**
- Dword 0 and the low bits of dword 1: base address. Programs that take the address out mask the upper bits of dword 1 away (`lower.rs` 3466–3471). The ray-tracing descriptor uses 40 bits: dword 0 plus the low byte of dword 1 (`vs/bvh.rs` 17).
- Dword 1 bits 16–29: stride; swizzle bits above (`resource.rs` 64–65, 1472–1476).
- Dword 2: record count in elements. This is the one buffer field code commonly reads as a number: null-view tests and `GetDimensions` (`lower.rs` 3424–3427, 7216–7236).
- Dword 3: flags. Bits 28–29 set means bounds are checked in bytes; bits 28–31 = 8 marks a ray-tracing structure (`vs/bvh.rs` 78–85).
- **Descriptors built by the code around a root descriptor's address:** dword 0 = address low; dword 1 = address high with constant bits OR-ed in (`s_or_b32` or `s_bitset1_b32`); dwords 2 and 3 are constants (`resource.rs` 1181–1201).
- **The "made raw" idiom** of the upscaler's compiler (`resource.rs` 1472–1509): dword 1 AND 0xC000FFFF; dword 2 = count × max(stride, 1), the stride taken with `s_bfe_u32` offset 16 width 14; dword 3 AND 0xCFFFFFFF then OR 0x30000000.
- **Constant buffers.** Nothing in a CBV descriptor is read as a number (`lower.rs` 7156–7159). Shortcut in the source: a CBV descriptor is assumed to have a 16-byte stride when read per lane with an index (`lower.rs` 12377–12379).

**Sampler descriptor (4 dwords, the public `SQ_IMG_SAMP_WORD0–3`)** (`vs/emit/sampler.rs` 1–15):
- Word 0: address modes x/y/z in bits 0–8 (3 bits each); log2 of max anisotropy in 9–11; comparison function in 12–14; reduction in 29–30 (0 filter, 1 min, 2 max).
- Word 1: min LOD bits 0–11, max LOD bits 12–23, unsigned 4.8 fixed point.
- Word 2: LOD bias bits 0–13, signed 5.8; mag filter 20–21, min filter 22–23 (point, linear, aniso point, aniso linear); mip filter 26–27 (none, point, linear).
- Word 3: border colour type in bits 30–31.
- Address-mode codes 0–7 map to wrap, mirror, clamp, mirror-once, then the half-border modes approximated.
- No program reads sampler dwords as numbers (`lower.rs` 7032–7034).
- **Comparison belongs to the instruction, not the sampler.** Every sampler carries a comparison function (less-or-equal when unused), so the host needs a comparison sampler wherever an `image_sample_c*` uses it.

**How descriptors travel in the code:**
- **Reloading.** The compiler loads a descriptor again wherever it needs it, and for sizes often loads dwords that are never used as a descriptor (`lower.rs` 6980–6986).
- **Parked in VGPR lanes** with `v_writelane`/`v_readlane` (section 7).
- **Kept per lane in VGPRs** after a per-lane branch, then read back with `v_readfirstlane` in a waterfall loop (section 7).
- **Chosen by control flow or by SCC** (`s_cselect`, `s_cmov`).
- **Loaded per lane** with two `global_load_dwordx4` (`resource.rs` 59–97, 1715–1717).

**What VIctory must be told by the host at bind time** shows what the descriptor would otherwise supply:
- per heap slot, the element stride and whether the view is of an integer format, signed or not;
- per heap slot, the title address of buffer views (`vs/emit/shader.rs` 16–45).

## 4. Memory loads

**Address shapes** (`resource.rs` 890–994, 1091–1114):
- `s_load_dword*`: an SGPR pair (64-bit base) + a 21-bit immediate + an optional offset SGPR. VIctory adds the offset SGPR as bytes.
- `global_load_*` / `global_store_*` / `global_atomic_*`: either an SGPR pair plus a VGPR offset plus a signed immediate, or a VGPR pair plus the immediate.
- Additions on the low dword wrap modulo 2^32. A carry into the high dword appears as `s_addc_u32` or `v_add_co_ci_u32`.
- Shortcut in the source: with a computed term, anything added to the high half is taken to be that term's carry, i.e. offsets below 4 GB (`resource.rs` 981–982).

**Address ranges assumed:**
- Heap addresses fit in 32 bits (`lower.rs` 8336–8337).
- Raw title addresses are 40 bits. The directory covers 2^21 pages of 64 KiB, addresses below 128 GiB (`vs/emit/shader.rs` 47–69).

**Where addresses come from:**
1. **A table or heap parameter:** this is a descriptor load.
2. **A root descriptor's address pair.** Read either through a built buffer descriptor (`s_buffer_load_dword*`, `buffer_load_*`, `tbuffer_load_format_*`) or directly with `global_load`.
3. **`s_getpc_b64`:** a constant table the compiler appends after the code. The base is the byte offset of the instruction after `s_getpc_b64`; an index is added, with `s_addc_u32` on the high half. Two tables may be picked by control flow and joined before one load (`resource.rs` 36–39, 718–723, 952–966). Out-of-range indexes read the last dword in VIctory (a shortcut; `lower.rs` 2155–2156).
4. **Raw title memory** from root constants, descriptor fields or other memory. Compute only in VIctory: the ray-tracing builder and the programs that walk its structures (`resource.rs` 41–57).

**Constant buffers.**
- Read with `s_buffer_load_dword[x2/x4/x8/x16]` through a 4-dword descriptor at byte offset = immediate + offset SGPR.
- Per lane with `buffer_load_dword*`: index × 16 + offset (`lower.rs` 6699–6930, 12376–12392).
- The console aligns constant buffers on 16 bytes; the host needs 256 (`lower.rs` 7384–7390).

**`MUBUF` addressing** (`lower.rs` 12312–12375):
- Byte = [index VGPR × stride if `idxen`] + [offset VGPR if `offen`] + 12-bit immediate + offset SGPR.
- The stride is the descriptor's; a raw buffer has stride 1, so "the index is the byte".
- With both flags the index is the first address VGPR and the offset the second.

**Narrow loads and stores.** Bytes and shorts are naturally aligned inside one dword, except that a 16-bit value at a dword's last byte continues into the next. `_d16` forms carry two 16-bit values per VGPR, low half first (`lower.rs` 12284–12293, 12428–12433).

**Typed buffer loads** (`tbuffer_load_format_*`). The format is in the instruction (LLVM's `UfmtGFX10` numbering, 14 families). Channel widths run from the lowest bits up; channels the format lacks read 0, 0, 0, 1. Normalised formats must give exactly code / (2^w − 1) rounded to nearest (`lower.rs` 212–250, 7463–7632).

**One multi-dword load is one read.** Its address + 4, + 8, … does not wrap at 32 bits. Loads from different instructions must not be merged, because their 32-bit offsets may wrap (`lower.rs` 6665–6673).

**Cache-invalidate instructions** (`buffer_gl0_inv`, `buffer_gl1_inv`) name a buffer but read nothing; VIctory drops them.

## 5. Stage inputs

**Stage data layout** (offsets from E = record + 0x54; `vs/native.rs` 31–61):

| Stage | Offset | Content |
|---|---|---|
| PS | E+0x24 | 32 input entries of 8 bytes: usage, signature row, channel mask (low 4 bits), attribute number (the `attr` of `v_interp`), default, second row of a packed pair, parameter slot, flags (bit 0 flat, bit 1 linear, bit 3 fp16, bit 4 two fp16 per dword) |
| PS | E+0x124 | u32 input count |
| PS | E+0x134 / E+0x138 | `SPI_PS_INPUT_ADDR` / `DB_SHADER_CONTROL` |
| PS | E+0x148 | Export patch size in dwords (≤ 10) |
| PS | E+0x14C | 8 patches (one per MRT), 404 bytes each: u32 code offset in dwords (0xFFFFFFFF = none), then 10 variants × 10 dwords |
| VS | E+0x24 | 32 input entries of 5 bytes: usage, row, channel mask, VGPR of channel 0, highest channel |
| VS | E+0xC4 | u32 input count |
| VS / MS | E+0xC8 | 32 output entries of 6 bytes: usage, row, parameter number, row again, slot, flags (bit 0 fp16, bit 1 two fp16 per dword) |
| VS / MS | E+0x188 | u32 output count |
| CS | E+0x24 | `COMPUTE_PGM_RSRC2` |
| CS | E+0x28 | Thread-group size, 3 × u32 |
| MS | E+0x18C | Eight register values: `SPI_VS_OUT_CONFIG`, `SPI_SHADER_POS_FORMAT`, `PA_CL_VS_OUT_CNTL` (at E+0x194), `VGT_STRMOUT_CONFIG`, `SPI_SHADER_PGM_RSRC2_GS`, `VGT_GS_OUT_PRIM_TYPE`, `VGT_GS_INSTANCE_CNT`, `SPI_SHADER_IDX_FORMAT` |
| MS | E+0x448 | u16 thread-group size in lanes, u16 max vertices, u16 max primitives, u8 user SGPRs, u8 topology (1 lines, 2 triangles); code follows at E+0x45C |

- Usage codes: 6 generic, 7 clip distance, others kept raw. "Row" is a register of `ISG1`/`OSG1` (`PSG1` for per-primitive).
- In MS output entries, bit 7 of the parameter byte marks a per-primitive output.
- The 2020 compiler leaves a mesh program's first output entry zeroed; its row is the one no other entry names (`lower.rs` 12803–12804).

**Vertex (hardware VS)** (`vs/io.rs` 14–17, 213–229):
- v0 = vertex id, v2 = primitive id, v3 = instance id.
- Attributes are pre-loaded by a fetch shader: channel k of an attribute arrives in VGPR (entry's VGPR + k), for the channels in the mask. `s_swappc_b64` in the program is the call to that fetch routine (`lower.rs` 6118–6119).
- Some vertex and mesh programs instead fetch vertices themselves with `tbuffer_load_format_*` through descriptors built around a root SRV (stride 1, no record limit). Index, stride and attribute offset are then the shader's own arithmetic on headers in that buffer (`vs/emit/mod.rs` 32–41).
- The console signature leaves out vertex and instance id rows.
- Shortcut in the source: 16-bit vertex inputs are not unpacked.

**Pixel** (`vs/io.rs` 86–116, 515–533). VGPRs are assigned from v0 upward, in bit order of `SPI_PS_INPUT_ADDR`, only for set bits:

| Bit | Input | VGPRs |
|---|---|---|
| 0 / 1 / 2 | Perspective sample / center / centroid (i, j) | 2 each |
| 3 | Perspective pull model (i, j, w) | 3 |
| 4 / 5 / 6 | Linear sample / center / centroid (i, j) | 2 each |
| 7 | Line stipple | 1 |
| 8–11 | Position x, y, z, w | 1 each |
| 12 | Front face (code tests it ≠ 0) | 1 |
| 13 | Ancillary (sample id and other bits) | 1 |
| 14 | Sample coverage | 1 |
| 15 | Fixed-point position | 1 |

Interpolation:
- `v_interp_p1_f32 dst, i, attr.chan` then `v_interp_p2_f32 dst, j, attr.chan`. The second reads the first's result as its old destination.
- The f16 forms are `v_interp_p1ll_f16` and `v_interp_p2_f16` (attribute operand first, old value as third operand).
- `v_interp_mov_f32` reads one vertex's value: constant 2 = P0 (provoking vertex), 0 = P10 (P1 − P0), 1 = P20 (P2 − P0).
- Hardware evaluation: P0 + i·P10 + j·P20, so i and j weigh vertices 1 and 2 (`io.rs` 129–146; `lower.rs` 3151–3160).
- The attribute number maps to a signature row through the PS input table.
- The compiler may share a first half across paths, or compute i/j itself; then each half must be evaluated literally.
- A first half's result is occasionally read as data. An f16 second half that writes only the high half keeps the low half the first one wrote (`lower.rs` 2073–2078, 11058–11063).
- Shortcut in the source: triangles only; lines and points not checked.

**16-bit interpolants.** Each 16-bit element gets whole dwords unless packed two per dword. The packing rule (`lower.rs` 1626–1640): a parameter leads with the first element of its first row, then of its second row; an element sharing a register behind another is not in that row's halves but follows in the first parameter with whole dwords left. The PS table may omit the second row; it is then the next unnamed 16-bit row.

**Compute** (`vs/io.rs` 22–23, 230–244). Thread id within the group in v0..v2 (as many as RSRC2 bits 11–12 say); group ids and group info in SGPRs after user data (section 1); group size from stage data.

**Mesh** (`vs/io.rs` 25–32). One NGG stage:
- The program computes its thread index from the wave index (s3 bits 24–27) and its lane (`v_mbcnt`).
- Its group id comes from `v_readfirstlane` of v5; the code divides by max(threads, vertices, primitives) (`vs/emit/mod.rs` 780–784).
- The console numbers mesh groups along x only.

## 6. Stage outputs

**`exp` targets** (`vs/io.rs` 429–449): 0–7 = MRT n; 8 = mrtz; 9 = null; 12–16 = pos0–pos4; 20 = primitive; 32–63 = param(n − 32). Each instruction enables up to four channels, has a `compr` bit (two f16 per dword; VIctory refuses "a compressed export" after patching) and a `done` bit.

**mrtz channels** (`lower.rs` 12869–12877): 0 = depth, 1 = stencil reference, 2 = sample mask, as `DB_SHADER_CONTROL` enables.

**Position exports:**
- pos0 = clip-space position.
- pos1 and pos2 = clip then cull distances in semantic order, 4 per export, when there is no misc vector. This is a shortcut in the source: point size, render-target index and viewport index are refused (`lower.rs` 12890–12910).
- Channels beyond the declared distances are ones `PA_CL_VS_OUT_CNTL` does not enable.

**Parameter exports.**
- `param<n>` maps through the VS/MS output table to an output signature row; the pixel side reads the same row through its attribute table.
- A vertex shader with no outputs still exports param0, because the hardware wants one.
- fp16 outputs: one 16-bit value in the low half of each dword, or two per dword with the layout of section 5.

**Colour-export patches** (`vs/native.rs` 39–42, 183–198, 779–802).
- The stored code has, per render target, a patch site of "patch size" dwords at the given code offset.
- The stage data carries ten variants per target, indexed by export format: ZERO, 32_R, 32_GR, 32_AR, FP16, UNORM16, SNORM16, UINT16, SINT16, 32_ABGR. The console writes the variant for the bound target format at pipeline creation.
- VIctory always applies variant 9 (32_ABGR), so exports carry four raw 32-bit values and the host converts.
- An unpatched program's exports are whatever was stored; you must patch before lifting.

**Discard.** There is no kill instruction in this model. A lane whose EXEC bit is clear at the export marked `done` is discarded. A lane that leaves the program without its final export was killed (`vs/io.rs` 10–12; `lower.rs` 12788–12798, 13167). VIctory accepts a conditional export only for a pixel `done` export.

**Mesh** (`vs/io.rs` 28–32; `lower.rs` 13013–13017, 5178–5229).
- Lane n exports vertex n (pos, param) and primitive n.
- `exp prim` has three 10-bit corners at bits 0, 10, 20: a 9-bit vertex index with an edge flag above it (bits 9, 19, 29). Bit 31 culls.
- One lane sends `s_sendmsg` id 9 (GS_ALLOC_REQ) with m0 = vertices | primitives << 12.
- Shortcut in the source: VIctory declares the maxima and culls every primitive first.
- The console never exports a primitive id or cull flag as outputs.

**Primitive-ordered pixel shading** (`lower.rs` 132–149). The compiler turns rasterizer-ordered views into a section that begins with `s_setreg_b32` to hardware register 25, reads the operand `src_pops_exiting_wave_id` (code 239), and ends with `s_sendmsg` id 7. VIctory drops the waiting and marks UAVs inside as rasterizer-ordered.

## 7. Wave-level behaviour

**General model** (`vs/structure.rs` 1–17; `lower.rs` 1–7). Each register dword is a 32-bit integer per invocation. Lane masks exist only as per-lane booleans (a BDD over atoms: a compare result's lane bit, a uniform SCC, a joined mask, a quad-widened mask, an opaque mask, a constant mask). A vector write under partial EXEC keeps the old value, so it becomes a guarded write.

**Masks read as numbers** (`s_bcnt1`, `s_ff1`, `v_mbcnt`, scalar compares, bit tests) become ballots taken where the mask is written (`lower.rs` 408–426):
- Compute: exact, because the program declares the console's wave size and relabels threads so host lane n is console lane n.
- Pixel and vertex: only wave-size-independent uses allowed (counts, first lane, count below a lane, equality).
- Mesh: no wave size can be declared in SM 6.6, so cross-lane operations are refused except a few.

**Whole-wave sections** (`vs/emit/wave.rs` 1–12, 141–160).
- Code between `s_or_saveexec_b64 x, -1` (or `s_nand_saveexec 0`) and the restore of the saved mask runs in every lane, inactive ones included. This is how the compiler expands HLSL wave reductions and prefix sums.
- The expansion puts an identity into inactive lanes with `v_cndmask`, then uses DPP row shifts, `v_permlanex16` and `v_readlane` of the last lanes.
- VIctory simulates the section symbolically per console lane and emits the matching wave intrinsic (active sum/min/max/and/or/xor, prefix sum inclusive or exclusive). Anything not modelled is "unknown".
- Sums of f16 are not associative: the compiler's order is a balanced tree over lanes in order, and VIctory reproduces it (`lower.rs` 2643–2656).

**Waterfall loops** (`resource.rs` 176–193, 1966–1974, 2138–2150).
- For a resource index that differs per lane, the code reads the first active lane's value with `v_readfirstlane_b32`, compares every lane with `v_cmp_eq`, runs the access for the matching lanes, and repeats with the rest.
- The console compiler's form adds the first lane's table offset in the load itself (`s_load_dwordx8 .., s[4:5], s20`).
- VIctory proves the loop shape, then gives every lane its own descriptor with a non-uniform index, so one pass serves all lanes.
- Shortcut in the source: only the accesses are proven; other work in the loop is assumed to run once per lane.

**"Is this value uniform across the wave?"** (`lower.rs` 6035–6039). The pattern is `v_readfirstlane`, `v_cmp_eq_u32`, `s_xor` with EXEC, `s_bcnt1`. Both branches give the same result per lane, so any wave size may answer for itself.

**Individual instructions:**

| Instruction / pattern | Host form in VIctory |
|---|---|
| `v_readfirstlane_b32` | Value of the first lane where the guard holds (ballot + first bit + read lane at); with no such lane, the wave's first lane (`lower.rs` 5385–5435) |
| `v_readlane` / `v_writelane` with constant lane, VGPR used for nothing else | Not cross-lane at all: the compiler parks SGPRs in VGPR lanes when it runs out. Rewritten to scalar moves before SSA; pairs in neighbouring lanes move as 64-bit (`vs/ir.rs` 241–268) |
| `v_readlane` at a computed lane | A host lane in pixel/vertex, the console's in compute (`lower.rs` 9402–9403) |
| DPP quad permute (ctrl 0–0xFF), DPP8, `ds_swizzle_b32` with offset bit 15 | Stay inside a quad. In pixel shaders they become quad reads: lane-at within the quad, or across X / Y / diagonal (`lower.rs` 5329–5364, 8453–8539) |
| DPP row shifts/rotates/mirror, `v_permlane16`, `v_permlanex16` | Compute only. Confined by the ISA to 16-lane rows or one 32-lane half; `v_permlanex16` reads the other row of the same half, each nibble of {src2, src1} picking a lane (`lower.rs` 9326–9328) |
| `ds_bpermute_b32` | Touches no LDS: each lane takes the data of lane (address + offset) / 4; a source lane outside EXEC gives 0. Used for neighbour samples in checkerboard shadow masks (`lower.rs` 5550–5558) |
| `v_mbcnt_lo/hi` | Bits of the mask below this lane plus src1; with an all-ones mask, the lane index (`lower.rs` 10811–10817) |
| `v_cndmask_b32` on a constant bit pattern | The lane's own bit of the constant, by console lane number; exact only with console lanes (`lower.rs` 9255–9267) |
| `s_wqm` | "Some lane of the quad is set" is taken as true. A shortcut: wrong if such a section had side effects (`lower.rs` 4611–4613) |
| `s_quadmask` | Packs whether each group of four lanes has a set bit |
| `v_movrels` / `v_movreld` | M0-indexed VGPR access, from the encoded register up to the program's VGPR count (the record's count bounds it) |
| `s_barrier` | Group sync plus a shared-memory fence. The console's barrier is the wave's, and the compiler omits barriers for LDS shared within one wave (`lower.rs` 1826–1828; `vs/emit/mod.rs` 160–175) |

**The two AMD-related problems:**

1. **Helper lanes.**
   - The console's whole-quad mode keeps helper lanes in EXEC, so waterfall loops and first-lane reads serve them.
   - Host wave operations leave helper lanes out; AMD returns 0 from them. Only quad reads reach helper lanes.
   - Result before the fix: once only helper lanes were left in a waterfall loop, none ever matched; they ran to the iteration limit and left their quad without a sample for derivatives, visible along every triangle edge.
   - Fix: in a pixel shader a helper lane (the is-helper-lane query) takes its own value where the console reads the first lane's, and quad-local reads always go through quad operations (`lower.rs` 5320–5408; `vs/emit/mod.rs` 152–155).
   - Structurisation counts helper lanes as live.

2. **AMD driver compiler faults.**
   - It crashed on a front-face input declared without an interpolation mode; the fix is to declare it constant, as DXC does (`vs/emit/mod.rs` 749–750).
   - It hangs on divergent loops unless the exiting block is the latch, so empty latches are folded into their predecessor as in DXC's loops (`vs/emit/bitcode.rs` 716–718).
   - A loop exit testing the XOR of a compare hung it; negated conditions swap the branch targets instead (`lower.rs` 13138–13139).
   - Its `fcmp one` behaves as unordered (true for NaN); the fix is ordered-less OR ordered-greater (`lower.rs` 10942–10945).
   - On an RX 9070 XT the compiler crashes inside `amdxc64.dll`, not on the same pipelines every run. VIctory catches it with a vectored handler, lists the pipeline in a `warm-bad.txt` and skips it (`vg/pipeline/make.rs` 102–110, 192–199).
   - Creating all the title's pipelines inside the running title ended it inside the driver's compiler (`vg/pipeline/known.rs` 10–13).
   - **Not found:** any crash tied to specific pipeline *flags*. The nearest items are a pipeline-library load failure for mesh pipelines without a topology subobject (`vg/pipeline/host.rs` 936–946) and vkd3d refusing blending on integer targets (122–128).

**Wave size on the host.** A compute program with cross-lane code declares the console's wave size, so the device must support it. NVIDIA tops out at 32 lanes; much of `lower.rs` (exchange through LDS, "local waves", widening branches) exists to run wave64 programs as two 32-lane waves, with approximate fallbacks marked as such (`vs/emit/mod.rs` 475–609; `lower.rs` 1844–1851).

## 8. Control flow

**Graph building** (`vs/ir.rs`).
- Blocks come from the branch targets; scheduling-only instructions are dropped.
- A conditional branch to its own fallthrough is dropped as redundant.
- Irreducible loops (a rotated loop whose peeled first test jumps into the body) are fixed by node splitting before SSA (`ir.rs` 1140–1146).
- A far jump is `s_getpc_b64`, add, `s_setpc_b64`. `s_setpc_b64` to a register address otherwise is an indirect exit, e.g. the handover between halves of a merged stage.

**VIctory does not rebuild if/else/loop syntax.** It keeps the graph (which must be reducible) and turns each wave-level branch into a per-lane branch (`vs/structure.rs` 1–17, 1322–1372):

| Branch | Per-lane condition | Console's wave-wide rule |
|---|---|---|
| `s_cbranch_execz` | Lane not in EXEC | Taken when no lane is set |
| `s_cbranch_execnz` | Lane in EXEC | Taken when some lane is set |
| `s_cbranch_vccz` / `vccnz` | Lane's bit of VCC AND EXEC (negated for z) | None / some |
| `s_cbranch_scc0` / `scc1` after a mask operation | The lane's bit of that operation's result | "Some lane set"; after `s_cmp_eq` of two masks, "every lane equal" |
| `s_cbranch_scc*` after a scalar compare | A uniform condition | Same in every lane |

This is sound because compiled code only skips work that lanes outside the mask would not change, or tests a value that is the same in every lane.

**Mask operations followed** (`structure.rs` 1054–1191):
- `s_mov`, `s_not`, `s_and`, `s_or`, `s_xor`, `s_andn1`, `s_andn2`, `s_orn1`, `s_orn2`, `s_nand`, `s_nor`, `s_xnor` in the wave's width (b64 for wave64, b32 for wave32).
- `*_saveexec` forms: the destination gets the old EXEC; EXEC gets op(source, EXEC); SCC = result non-zero.
- `*_wrexec` forms: destination and EXEC both get the new mask.
- `s_cselect` on SCC, `s_wqm`, `v_cmp*` into VCC or an SGPR pair (result ANDed with EXEC), `v_cmpx*` writing EXEC directly, and carry-outs of add/sub.
- Inline constants 0 and −1 are none/all lanes; other constants are "constant masks".

**Patterns:**
- **If:** compare, `s_and_saveexec`, `s_cbranch_execz` over the body, then a restore (`s_or` with the saved mask).
- **Else:** `s_andn2` or `s_xor` with the saved mask.
- **Loop:** do { … } while (EXEC), closed by `s_andn2_wrexec` (or similar) and `s_cbranch_execnz` back. SCC written just before the closing branch is known clear after the loop (`structure.rs` 1130–1134).
- **Break:** removing lanes from EXEC and from a saved mask.
- **Early out:** `s_cbranch_execz` to a block that only ends the program. For long programs, `s_cbranch_execnz` over a far jump to the end (`ir.rs` 724–733).
- **Skips** of a region when no lane is left for it. The console then does not run that region's scalar code, which matters only if a scalar or SCC changed in the region is merged after it (`ir.rs` 789–799).

**Guards.** A path-sensitive pass proves, per vector instruction, that EXEC holds in every lane reaching it. What it cannot prove stays as a per-lane guard (`structure.rs` 415–418).

**Loop limit.** Every loop is cut after a limit, because unmirrored data can make a loop run four billion times and hang the GPU. Default 2^20; in practice 128 iterations for pixel, 256 for vertex/mesh, 4,096 for compute. All are marked as shortcuts (`lower.rs` 399–406, 13200–13210).

## 9. Image instructions

**Naming of descriptors** (`vs/gfx10/fields.rs` 382–386). The resource is the SGPR group starting at 4 × (bits 16–20 of the second word): 8 dwords, or 4 with r128. The sampler, if the opcode samples, is the 4 dwords starting at 4 × (bits 21–25).

**Fields:** dmask, dim, unorm, a16 (16-bit addresses), d16 (16-bit data), tfe, lwe, glc/cache bits, plus NSA extra address words. VIctory refuses tfe and lwe.

**Dimension codes** (`lower.rs` 11410–11424, 7112–7120): 0 = 1D (1 coordinate), 1 = 2D (2), 2 = 3D (3), 3 = cube (3: face s, face t, face id), 4 = 1D array (2), 5 = 2D array (3). Codes 6–7 (MSAA) and cube loads are refused.

**dmask.**
- Loads and samples: the enabled channels are written to consecutive result VGPRs in order. A dmask of 0 returns the first channel.
- Gathers: exactly one bit set, selecting the component; four results.
- With d16: two channels per VGPR, low half first.

**Address VGPR order** (`lower.rs` 11742–11762, 11842–11849):
1. offset word, if the name ends in `_o`;
2. bias, if `_b`;
3. compare reference, if `_c`;
4. gradients, if `_d`: all of dx then all of dy;
5. coordinates;
6. then one more: LOD for `_l`, mip for `image_load_mip`, or the minimum-LOD clamp for `_cl`.

- **a16:** coordinates, gradients, LOD and clamp are f16, two per VGPR low half first. Offset, bias and reference keep their own whole VGPR; a16 bias is an f16 in the low half.
- **Offset word:** x in bits 0–5, y in bits 8–13, signed 6-bit; z not used by VIctory.
- **unorm:** coordinates are in texels.
- **Array slice** is the last coordinate, as a float for samples.
- **Integer loads:** coordinates are integers; `image_load_mip` takes the mip after them.

**Opcode mapping used** (`lower.rs` 11656–11670, 11904–12126):

| Console | Host |
|---|---|
| `image_sample`, `_o`, `_cl` | Sample (implicit derivatives), pixel shaders only. Elsewhere VIctory samples level 0: a shortcut; the console takes derivatives from neighbouring lanes there |
| `image_sample_lz`, `_lz_o`, `_l` | Sample at level (0, or the LOD operand) |
| `image_sample_b`, `_b_cl` | Sample with bias (outside pixel: bias used as the level) |
| `image_sample_d`, `_d_cl` | Sample with gradients. Cube gradients are of the two face coordinates, so VIctory computes a level from the longer one; shortcut: no anisotropy |
| `image_sample_c`, `_c_cl`, `_c_lz` | Compare sample / compare at level zero |
| `image_gather4_lz`, `_lz_o` | Gather |
| `image_gather4_l`, `_l_o` | The host gather has no level, so four texel loads at the rounded level in the order (u0,v1) (u1,v1) (u1,v0) (u0,v0); the offset is in texels of that level |
| `image_gather4` (implicit level) | Refused |
| `image_load`, `image_load_mip` | Texel load |
| `image_store`, `image_store_mip` | Texel store. Shortcut: assumed to write all four channels of a float format |
| `image_get_resinfo` | Dimensions of the level in the first address: width, height, depth or layers, levels, per dmask; channels the kind lacks read 1 |
| `image_get_lod` | Clamped then unclamped LOD (pixel only) |
| `image_atomic_*` | Atomics on an integer view. dmask 1 = 32-bit, 3 = 64-bit; cmpswap takes replacement then comparand; `sub` is an add of the negation; glc returns the old value |
| `image_bvh_intersect_ray`, `image_bvh64_…` | Ray/node test; the "image" descriptor names a memory range |

**Cube sampling** (`lower.rs` 12198–12203). The code computes face coordinates with `v_cubesc`/`v_cubetc` over |`v_cubema`| plus 1.5 (s and t in [1, 2]) and the face with `v_cubeid`; the host needs the direction back. Major axis rule: z when |z| ≥ |x| and |z| ≥ |y|, else y when |y| ≥ |x|, else x. Shortcut: cube arrays not handled (the face operand adds 8 × slice).

**Not found:** MSAA sample-index handling, `image_sample_cd` variants, and the meaning of the unorm bit on loads.

## 10. Host side in VIctory

**Root signature: per pipeline**, built from what the translated shaders read (`vg/pipeline.rs` 1–6 of `pipeline/host.rs`; `pipeline/host.rs` 473–596). Per stage:
- One block of 32-bit root constants at b0 space 0 holding the user data. Its dword index is the console user-data index (minus 16 for spilled programs). A root signature holds 64 dwords in total.
- The title's root CBV/SRV/UAV parameters at b/t/u<title root slot> in space 1. Root SRVs and UAVs are bound as one-view tables by default, so reads past the end return zero: a root descriptor has no size and a mesh shader's vertex fetch has no record limit (`pipeline/host.rs` 347–352).
- Static samplers s<n> space 0, decoded from the immediate sampler words in the code. A constant used both with and without comparison takes two slots.
- Both heaps directly indexed (SM 6.6), so the host heap keeps the title's descriptor order slot for slot.
- Extra root views of its own: a per-slot stride/format table (slot 255), a 64 KiB-page directory of title memory (254), and a fault buffer (253).

**User-data values** (`vg/draw.rs` 1463–1489, 1848–1886): a table is its byte offset in its heap (index × 32, or × 16 for samplers); a constant is its value; heap bases are 0.

**Descriptor tables become** indices into the mirrored heap; there is no host table binding. The host slot for title slot i is made when the title writes it and again when a draw reads it through a table and finds it changed. A write-watch pass catches descriptors the title copied as bytes (`vg/descriptor/host.rs` 1–6; `vg/descriptor.rs` 362–369).

**Buffers.**
- All buffer views are made raw on the host; the shader multiplies by the stride. A raw read through a structured view gives zeros on NVIDIA; on AMD a raw load through a non-raw view has no bound and loses the device.
- A raw view must start on 16 bytes, so there is a "raw twin" region of the heap and per-view byte biases (`vs/emit/shader.rs` 84–91, 170–181; `lower.rs` 12361–12364).
- Constant buffers are host CBVs by default: copied to 256-byte-aligned upload memory at every submission, about 23,000 a frame. An optional mode reads them as raw bytes in place.

**At draw time** (`vg/draw.rs` 1714–1890), in order:
1. set both descriptor heaps, before the root signature, because directly indexed heaps require that;
2. set the root signature and pipeline;
3. per stage, walk the tables the shader reads via the used-resource list: make changed host slots, copy constant data, transition textures;
4. write the user-data constants;
5. bind root views;
6. snapshot and write back buffers the shader may store to.

Draws are dropped while a pipeline is still compiling or a table found no free slots.

**Pipelines.**
- Host pipelines are made on first draw, not at creation: the title creates about 60,000–84,000 pipelines and draws with about 2,000.
- Shader containers are shared by content hash (`vg/pipeline.rs` 6–10; `vg/pipeline/known.rs` 6–8).
- The console numbers amplification and mesh shader subobjects 23 and 24 (PC: 24 and 25) (`vg/pipeline.rs` 670–672).
- Console-only topologies: 19 = rectangle list (three vertices per rectangle; VIctory adds a geometry shader) and 32 = quads.
- Blending must be turned off on integer render targets (the console ignores it; D3D12 refuses).

**Samplers created by the title** use a console description of 27 words; comparison is used when the instruction says so (`vg/sampler.rs` 33–36, 102–105).

## 11. Other things the source learned the hard way

**Registers and constants**
- Unset VGPRs read as zero. `src_scc` as an operand reads SCC as 0 or 1 (`lower.rs` 4126).
- 64-bit inline integer constants sign-extend. A 32-bit literal in a signed 64-bit operand sign-extends too; in unsigned and bit operands it zero-extends. A 32-bit literal in a 64-bit *float* operand is the high 32 bits (`lower.rs` 9570–9577, 9620).
- Inline float constants 240–248 are 0.5, −0.5, 1, −1, 2, −2, 4, −4, 1/(2π). In an f16 operand the constant sits in the low half only. Packed integer instructions see the f32 bits (`lower.rs` 193–194, 4142–4145, 10273–10276).
- SOPK arithmetic reads its destination as its first source.
- `s_bfe`: offset in bits 0–4 of src1, width in bits 16–22.
- SGPRs are reused between lane masks and plain numbers (the ray tracer keeps masks on a stack with node numbers), so "is this a mask or a number" cannot always be decided per register. VIctory retries translation in a second mode for those programs (`vs/emit/mod.rs` 396–400; `lower.rs` 13347–13349).

**Float behaviour that differs from host intrinsics**
- `v_max_f32`/`v_min_f32` order −0 below +0.
- `v_med3_f32` with a NaN gives the minimum of the three.
- `v_fract_f32` never returns 1.0 (largest value below 1 instead).
- `v_sin`/`v_cos` take the angle in turns; half-turn sine and quarter-turn cosine are exactly +0.
- `v_mul_legacy` and the like: 0 × anything = 0.
- Float-to-int conversions saturate and give 0 for NaN.
- f16 operations round to nearest even on the console, while NVIDIA's f32-to-f16 conversion rounds toward zero.
- 16-bit results keep the other half of the destination register.
- `v_rcp` is approximate on hardware; VIctory divides.
- The float MODE register is not modelled.
- NVIDIA folds `fcmp uno` away under fast-math, so NaN tests are done in integer arithmetic.
- (`lower.rs` 4280–4520, 9052, 9091, 10011)

**Other instruction semantics**
- `v_div_fixup_f32`: src0 = value to scale, src1 = denominator, src2 = numerator.
- `v_perm_b32` selector bytes: 0–7 pick a byte of {src0, src1} with src1 the low dword; 8–11 repeat sign bits; 12 gives 0x00; higher gives 0xFF (`lower.rs` 8855, 10689–10692).
- `ds_max_f32`/`ds_min_f32` NaN rules were measured on real hardware (`lower.rs` 5797–5802).
- LDS out-of-bounds reads give 0 and writes are dropped. `ds_read2`/`ds_write2` scale their offsets by the element size (×64 for the st64 forms).

**Descriptors and memory**
- View indices read from data can be garbage. Reading past the host heap hung NVIDIA and removed the device on AMD RDNA 3, so indices are clamped to a null slot and sampler indices to 2,048 (`vs/emit/shader.rs` 153–157, 523–527).
- A buffer picked from the resource heap by a computed index has no used-resource entry. Reading it as a constant buffer gave nothing: meshlet culling passed no meshlets (`lower.rs` 6758–6770).
- Typed buffer stores of d16 data through an integer-format view must pass 16-bit integers, not floats (`lower.rs` 12576–12582).
- "A level-less gather of a depth pyramid" and "log2 versus natural log for gradient LOD" were both real bugs fixed (`lower.rs` 11995–11998, 12055–12068).
- A value with no host form flowing into a join silently became zero in about 40 programs; VIctory still allows it by default and flags it as a shortcut (`lower.rs` 13284–13292).

**Containers**
- A vertex shader container can hold two machine-code blobs: compiled for use with and without a geometry shader.
- Blob kind 0x0C is unpacked FSSM, seen in the executable's own debug-text shaders.
- An `ACCC` part (FSPX, version 1) replaces `DXIL` for the FidelityFX upscaler's compute shaders. Its packet has wave32 as bit 4 of a u16 at +0x12, the thread-group size as 3 × u16 at +0x0C, RSRC1 at +0x14, RSRC2 at +0x18 (`vs/native.rs` 460–482).
- PSV0 from runtime info version 2 on carries the mesh thread count; vertex output interpolation modes live in PSV0, not OSG1 (`vs/emit/mod.rs` 780–784, 834–836).
- 16-bit signature elements are declared 32-bit on the host.

**Structure builder**
- The ray-tracing structure builder's compute stages must run all or none: they feed each other through plain title memory, and a dropped stage made later ones store through garbage offsets into the title's own objects (`vg/pipeline.rs` 246–263).
- Its 1D programs are dispatched by thread count with a partial last group; lanes past the count must not run (`lower.rs` 93–99).

## Checked against the real title's programs (2026-11-28)

`riptide-shader`'s lift was built against 8,634 programs of the real title. What that confirmed and corrected, by counts over those programs:

**Confirmed**

- The stage data offsets for pixel, vertex, compute and mesh programs. The tables agree with the input and output signatures, and the number of colour-export patch sites equals the number of render target rows in all 3,297 pixel programs.
- A vertex program has inputs exactly when it calls the fetch routine (332 of 1,034). Attribute registers are 4, 8, 12 and 16, and the routine's address arrives in s[4:5].
- A table register is 32 bits and the code zeroes the high half itself. A root view is a 64-bit address in two registers with a descriptor built around it by the code. A sampler fixed by the root signature is four constant words in the code.
- Export patch variant 9 is four `s_nop` and an `exp` of the raw registers. The patch size is 6 words in every program.

**Corrected**

- The stored pixel code is not unpatched: every patch site holds variant 4, a conversion to half floats and a packed export.
- User data is not in s(N) counted from 0. A root parameter's user-data number is the scalar register number itself: parameters start at s8 in vertex, pixel and mesh programs and at s4 in compute programs. A spilled compute program's start at 16.
- Pixel-input flag bit 1 is set on 5,607 of 6,281 entries, so it reads as "interpolated", not "linear".
- The "row again" byte of an output entry differs from the row in most entries; what it means is not known.

**Still on trust**: that per-lane branching is sound, the vertex and instance id registers, the buffer bound-check modes, the packed format numbering, and every float edge rule. Nothing was measured on console hardware.
