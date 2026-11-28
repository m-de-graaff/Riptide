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
- **T6. Descriptors are read where they lie (D5).** A program gets at a resource by loading a descriptor from an address its inputs give it. The descriptor it loads is the 32-byte record `riptide-gpu` wrote there. A buffer's record gives the title address and size, which T5 turns into reads. A texture's record gives the place of its host view in one bindless host heap; the record gains that place when the view is made, in the eight bytes that now hold the resource pointer. Samplers likewise index one host sampler heap. So binding a table is no work on the CPU: the title's root arguments are passed through as numbers.
- **T7. The host root signature is one shape for every pipeline.** Root constants for the title's root arguments (addresses and numbers, as the console's registers get them), the page table, the bank buffers, and the two bindless heaps taken straight from the heap. No per-pipeline root signature is made on the host.
- **T8. Every translation is checked against an interpreter.** A reference interpreter runs the intermediate form on the CPU, lane by lane. Tests translate synthetic programs, run them on the software adapter, and compare with the interpreter (#19, S7). No game data is in the tests. The real title checks coverage only: every program it makes is decoded and lifted, and the session log counts what could not be.

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
