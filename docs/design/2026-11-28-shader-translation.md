# Shader translation

Decided 2026-11-28, after the real title showed what its shader programs are. It fills in decisions D5 and D7 of `2026-11-27-architecture.md` and covers issues #18 to #22. Nothing here is built yet except where a line says so.

## What was measured

Numbers and how they were taken are in the session logs of the runs; the facts that decide the design:

- **A program is a DXBC container with no DXIL bitcode.** The first pipelines the title makes hold vertex and pixel programs of about 1 KiB. Their parts are `SFI0`, `ISG1`, `OSG1`, `PSV0`, `HASH`, `DXIL` and `PRIV`. The `DXIL` part's bitcode field is 4 bytes long and does not start with bitcode. What the part holds is the console's additions, the compiled machine code among them. So the host cannot be handed the title's own intermediate code: the machine code has to be translated.
- **The signatures are there.** `ISG1`, `OSG1` and `PSV0` say what a program takes in, puts out and binds, in the PC's own formats.
- **Root signatures are the PC's format.** The host's own functions pack and unpack the title's (built, `riptide-gpu`).
- **The title makes pipelines by the thousand**, from a stream description and from two console methods whose descriptions are not understood yet (device positions 57 and 67).
- **Descriptors are title memory.** The title places its view heap of 1,245,184 descriptors in its own memory and covers part of it with a buffer resource as well.

## Decisions

- **T1. Four stages, each usable alone.** Read the container; decode the machine code into instructions; lift instructions into a small intermediate form with structured control flow; emit a host shader. Each stage has its own tests and its own failure report, so a program that fails says where.
- **T2. The intermediate form is per lane.** The console GPU runs a program on 64 lanes at once (one per pixel or vertex) with a mask of live lanes. The host runs one invocation per lane. A vector register becomes a variable of the invocation; the live-lane mask becomes control flow; a scalar register holds either a value that is the same for every lane (a constant, an address, a descriptor) or a mask, and the lift tells which from how it is used. Instructions that look across lanes (wave reads, lane permutes) are lifted to the host's wave intrinsics, which is where issue #21's two AMD faults have to be tested.
- **T3. The first emitter writes HLSL, compiled by the DirectX shader compiler.** The reason is reach and proof: HLSL can be read, compared with what the program was meant to do, and fixed by looking at it. Writing DXIL bitcode directly (what the issue's title names) saves the compiler and its time, and is a second emitter behind the same intermediate form. It is written when a measurement of translation time asks for it, as D6 says of a command cache. The compiler (`dxcompiler.dll`, with `dxil.dll` to sign) is looked for beside `riptide.exe` and then in the Windows and Vulkan SDKs, and preflight says if it is missing.
- **T4. Translation is ahead of time, by content.** A translation is kept under the hash of the program's bytes and the translator's version. `riptide prepare` fills the cache from the title's files (#22); a program first seen while running is translated on a background thread and its draws are skipped until it is ready (D7, the `missing_pipeline` setting).
- **T5. Memory is read where it lies.** A program's loads from memory are loads from title memory. Each bank of title memory is a host buffer (built), bound as one array of raw buffers. A title address is turned into a bank and an offset through a page table the shader reads: one 32-bit entry per 64 KiB of title addresses, kept up to date from `riptide-mem`'s own table. This is M4 on the GPU, and it answers Q6 by not needing one view over everything.
- **T6. Descriptors are read where they lie (D5), and have the console GPU's own shape.** A program gets at a resource by loading a descriptor from a table whose address its inputs give it, and compiled code then reads fields out of what it loaded as plain numbers: an image's width, height, levels, kind and depth, a buffer's element count and sometimes its address (`docs/research/2026-11-28-console-shader-convention.md`, section 3). So the record `riptide-gpu` writes for a view is the hardware's descriptor in every field code reads, and the translator needs no special case for those reads: they are loads from memory like any other. The fields the hardware keeps for itself and code only passes on (an image's address, format and tiling words; a sampler's border colour pointer) are free, and hold what the host needs: the place of the texture's host view in one bindless host heap, and of the sampler in one host sampler heap. A buffer's descriptor is 16 bytes of a 32-byte slot; the other 16 are Riptide's. This replaces the record layout `riptide-gpu` has now.
- **T6a. Descriptor heaps lie below 4 GiB.** A table reaches a program as a 32-bit address, and the code makes the high half zero itself. The title places its own view heap below 4 GiB for this reason. A heap Riptide commits for the title must lie there too.
- **T7. The host root signature is one shape for every pipeline.** A program's inputs are the console's "user data" registers: root constants as their values, a root view as its 64-bit title address, a table as the 32-bit address of its first descriptor, the two heaps as their 32-bit addresses. Each draw writes them, per stage, into a small block of memory the GPU reads (a ring in memory the CPU writes at video-memory speed), bound as one root view. Beside it: the page table, the bank buffers, and the two bindless heaps taken straight from the heap. No per-pipeline root signature is made on the host, and binding a table is no work on the CPU.
- **T8. Every translation is checked against an interpreter.** A reference interpreter runs the intermediate form on the CPU, lane by lane. Tests translate synthetic programs, run them on the software adapter, and compare with the interpreter (#19, S7). No game data is in the tests. The real title checks coverage only: every program it makes is decoded and lifted, and the session log counts what could not be.

## What a translated program must get right

From the research note, the things that are easy to get wrong and are tested for by name:

- A pixel program's colour exports are patched at pipeline creation for the render target's format; the stored code is unpatched, and the patch for four raw 32-bit values is applied before lifting.
- There is no discard instruction. A lane whose live bit is clear at the final export is discarded.
- Comparison belongs to the sampling instruction, not the sampler: a sampler used by a comparing instruction needs a comparison sampler on the host.
- The compiler parks scalar registers in lanes of vector registers when it runs out, which is not a cross-lane operation; and it expands wave reductions into whole-wave sections, which are.
- Waterfall loops over a per-lane resource index, and helper lanes in pixel programs, which the host's wave operations leave out (issue #21).
- Float behaviour that differs from the host's: minimum and maximum order the two zeroes, `v_mul_legacy` gives zero for zero times anything, conversions saturate, sine and cosine take turns.

## The host shader's bindings

What every translated shader is given, and where. The emitter in `riptide-shader` writes shaders against this, and `riptide-gpu` binds it. Shader model 6.6, with both descriptor heaps indexed directly.

| What | Where | Holds |
| --- | --- | --- |
| Stage inputs | constant buffer `b0`, in space 0 for a vertex or mesh program, space 1 for a pixel program, space 2 for a compute program | 64 words: the console's user-data registers for that stage, in their order (a root constant's value; a root view's address, low word then high; a table's or a heap's 32-bit address). Then 16 words of what the stage is told besides: for compute, the thread-group counts. |
| Page table | raw buffer `t0`, space 0, as a root view | One word per 64 KiB of title addresses below 2^37: the frame mapped there plus one, or zero. A frame is its bank times 32,768 plus its place in the bank. |
| Banks, to read | the resource heap, places 0 to 15 | A raw buffer over each bank of title memory. |
| Banks, to write | the resource heap, places 16 to 31 | The same buffers for unordered access. |
| Textures | the resource heap, from place 64 | Each host view of a title's texture, at the place its descriptor record names. Place 32 is a view of nothing, for a record that names none. |
| Samplers | the sampler heap | Each host sampler, at the place its descriptor record names. A sampler that an instruction compares with is at that place plus 1,024, as the comparing form of the same sampler. |

Reading title memory at an address is: the page table's word for the address's page; nothing mapped reads as zero; else the bank and the offset in it; then a load from that bank's buffer. A write is the same through the unordered-access view, and a write where nothing is mapped is dropped.

A vertex program's attributes come from the host's own input assembler: the pipeline is made with the title's input layout, and the shader declares its inputs by the names and indices in the program's input signature. Values pass from one stage to the next by the rows of the two programs' signatures. A pixel program's colours go out as four raw 32-bit words per render target, declared as floats, unsigned or signed integers by the target's format, which the pipeline knows when it is made.

## Order of work

1. Container and decoder (#18). In progress.
2. Intermediate form, lift and interpreter, with the instructions the title's first pipelines use.
3. HLSL emitter, the host root signature, and draws in `riptide-gpu`: the first drawn pixel.
4. Coverage: more instructions, by how often the real title uses them. Texture conversion (#25) alongside, since a sampled texture needs it.
5. The prepare step (#22), background compiling and the pipeline library (#27).

## What is not decided here

- How texture layouts are converted on the GPU (#25). The first sampled textures can be converted on the CPU with `riptide-formats`, which is slow and correct.
- Whether the direct DXIL emitter is needed (T3).
- The two console pipeline descriptions (device positions 57 and 67).
- Ray tracing. The console's structure builder ships as its own module with its own programs.
