# riptide-shader

Console shader code (RDNA 2) to host shaders, including decoding resource descriptors inside the shader.

## Interface

The goal of the crate is to translate one shader from its bytes to a host shader plus a description of what it reads, keyed by content hash, knowing nothing of D3D12 objects. The design is `docs/design/2026-11-28-shader-translation.md`. All four of its stages exist: reading the container, decoding the machine code, lifting it into an intermediate form (with a reference interpreter for that form), and writing a host shader as HLSL. Everything is a pure function of its input, with no GPU or OS dependencies, no other crates and no unsafe code; the crate does not run a shader compiler. Malformed input gives an error, never a panic. (The tests are the exception: some run the DirectX shader compiler and a D3D12 device, and take the `windows` crate for that.)

`riptide_shader::container` reads the DXBC container:

- `Container::parse` splits a container into its parts by four-letter name (`FourCc`, `Part`).
- `Container::input_signature`, `output_signature` and `patch_signature` read `ISG1`, `OSG1` and `PSG1` into a `Signature` of `SignatureElement`s: semantic name and index, register, component mask, component type, system value.
- `Container::pipeline_validation` reads `PSV0` into a `PipelineValidation`: the kind of shader, the facts particular to its stage (`stage_details`), the wave lane counts, the thread group size, the entry point name, the resource bindings (type, register space, first and last register, resource kind, flags) and the signature rows.
- `Container::xbox_dxil` reads the `DXIL` part into an `XboxDxil`: the program header, and on the console the table of blobs after it, each `Blob` with its `BlobKind`. `XboxDxil::compiler_options` returns the compiler options blob as bytes.
- `XboxDxil::machine_code` finds the machine code blobs, unpacks the packed ones, and returns each as a `MachineCode` holding one `Program` per hardware stage: the stage, the wave size, the register counts, the root parameters and used resources, and the code as 32-bit words.
- `MachineCode::stage_data(&program)` reads the data that follows a program's record into a `container::stage::StageData`: a pixel program's inputs, `SPI_PS_INPUT_ADDR`, `DB_SHADER_CONTROL` and colour-export patches (`PixelData`); a vertex program's attributes and outputs (`VertexData`); a compute program's `COMPUTE_PGM_RSRC2` and thread group size (`ComputeData`); a mesh program's outputs and limits (`MeshData`). `PixelData::patched_code(code, ExportFormat)` returns the code with every colour export patched to one of the ten variants; `PixelData::system_inputs` lists which system inputs arrive in which vector registers.
- `Container::feature_flags` and `shader_hash` read `SFI0` and `HASH`.

`riptide_shader::inflate` unpacks zlib streams (`zlib`, `deflate_raw`, `adler32`). The machine code is stored as one, and the workspace takes no dependency for it.

`riptide_shader::gfx10` decodes the machine code of the GPU generation called GFX10:

- `decode_at(code, index, wave)` decodes the instruction at one word into an `Instruction`: its `Format`, opcode number, mnemonic, words, operands (`Arg`: a `Role` and an `Operand`) and the other fields of its format (`Detail`). It never fails. An instruction it cannot name keeps its words and has no mnemonic.
- `walk(code, wave)` decodes a program by following its control flow from the first word, because compilers put data after the end of a program. `decode_linear` decodes word after word.
- `Instruction` implements `Display` in the syntax of LLVM's assembler, and `listing` prints a list of instructions with offsets.

`riptide_shader::ir` is the intermediate form, written per lane (decision T2):

- A `Function` is a control-flow graph of `Block`s of `Inst`s, each an `Op` that defines consecutive temporaries (`Temp`), ending in a `Terminator` (`Jump`, `Branch`, `Return`). Values (`Value`) are 32-bit words or per-lane flags; a 64-bit address is an `Address` of two words. State across blocks lives in variables (`Var`): the vector registers, the scalar registers as numbers, lane masks as the lane's own flag (`Var::Mask`, `Var::Exec`, `Var::Vcc`), the condition code. `Function::variables`, `temp_types`, `has_cross_lane` and `verify` describe and check a function, and `Function` implements `Display`.
- The operations: `Unary`, `Binary`, `Ternary` and `Compare` with the console's exact semantics named in `UnaryOp`, `BinaryOp`, `TernaryOp` and `CompareOp` (the legacy multiply, minimum and maximum that order the zeroes, saturating conversions, sine and cosine in turns, the fraction that never reaches one, conversions to 16-bit floats in two rounding modes, the byte permutation, cube faces); `Select`, flag logic, `Carry` and `Borrow`; `Input` of a stage input (`Input`: user-data word, vertex and instance number, vertex attribute by signature row and channel, barycentric weights, a pixel attribute's per-vertex terms or its interpolated value, position, front face, thread and group numbers); `LoadMemory` and `StoreMemory` by title address in units of 8, 16 or 32 bits; `LoadBuffer`, `StoreBuffer`, `LoadFormat` and `StoreFormat` through a buffer descriptor given as its four words; `LoadCode` for tables of constants after the code; `Image` (`ImageOp`: sample, gather, load, store, size and level queries, with the dimension, the texture's host heap place and format bits as values, and the `Sampler` as a heap place or as the four words of a sampler the root signature fixes); `Export` to an `ExportTarget`; `Discard`; and the cross-lane operations `FirstLane`, `ReadLane`, `CountLanes`, `FirstLaneIndex`, `AnyLane`, `LanesBelow`, `BallotWord`, `LaneBit`, `QuadPermute` and `QuadAny`.
- `ir::eval` defines what the operations compute as plain functions (`unary`, `binary`, `ternary`, `compare`, `float_class`, `carry`, `borrow`, `half_from_float`, `float_from_half`), how a buffer descriptor's words give an address and a bound (`BufferView`), and the data formats of typed buffers (`BufferFormat`).
- `ir::cfg::ControlFlow::of(&function)` gives what an emitter of structured code needs: predecessors, a reverse post-order, immediate dominators, back edges, loops with header, body and latches, and whether the graph is reducible. `ir::cfg::make_reducible` splits nodes until it is, and `ir::cfg::remove_dead` removes writes and work nothing reads.

`riptide_shader::lift::lift(&program, &stage_data)` lifts one program to a `Function`, or returns a `LiftError` naming the instruction and the reason. It patches a pixel program's colour exports to four raw 32-bit values first. The function it returns is reducible, verified, and has had dead writes removed. How a wave program becomes a lane program is in the module's documentation.

`riptide_shader::interp` runs things on the CPU:

- `interp::run_wave(&function, lanes, &mut host)` is the reference interpreter of the intermediate form for one wave, with inputs, title memory, textures and exports supplied through the `Host` trait. It resolves cross-lane operations across the lanes that reach them together.
- `interp::machine::run_machine(&program, &stage_data, lanes, &mut host)` interprets the machine instructions themselves, a wave at a time, with the same `Host`. It exists so that tests can show the lift keeps a program's meaning.

`riptide_shader::hlsl` is the emitter (its items are also at the crate's root):

- `translate(container_bytes, stage, &Options)` reads a container, lifts the program of one hardware stage and returns a `Shader`: complete HLSL with the entry point `main`, the profile to compile it for (`vs_6_6`, `ps_6_6`, `cs_6_6`), the program's root parameters, the samplers its root signature fixes (`samplers`, as their four hardware words), how many rows it passes or takes (`rows`), which rows a pixel shader reads as bits that must not be interpolated (`flat_rows`), and whether its control flow had to be written as a dispatch loop (`dispatched`, never so for a lifted program). A `TranslateError` names the step that failed (`read`, `decode`, `lift` with the `LiftError`, `emit` with an `EmitError` that names the operation) in one line.
- `Options::targets` says, for each render target a pixel program writes, whether its format takes floats, unsigned or signed integers (`TargetKind`); the four words exported to it are declared and converted accordingly.
- `emit_function(&function, inputs, outputs, &Options)` is the emitter alone, for a function lifted elsewhere.
- `rectangle_geometry_shader(rows, flat)` writes the geometry shader that completes a console rectangle from the three corners the title draws: the fourth is `a + b - corner` for the position and every row, except the rows of `flat`, which are copied from the first vertex.
- `ARGUMENTS` are the compiler arguments the shaders are written for (`-HV 2021 -Gis -denorm preserve`), and `VERSION` changes whenever the text emitted for a program could change.

The shader is written against the bindings of the design ("The host shader's bindings"), which `riptide-gpu` provides, and reads these stage input words beyond the 64 user-data words: 67 to 77, the places in the sampler heap of the samplers the root signature fixes, in the order of `Shader::samplers`; 78, the draw's first instance; 79, its first vertex. The emitter's own documentation (`src/hlsl/mod.rs`, `emit.rs`, `structure.rs`, `image.rs`, `helpers.rs`) says how a function becomes text.

`riptide_shader::summarize(container_bytes)` reads a container, decodes every program in it and returns a `Summary` of counts, which the runtime can log for every shader of a real title without any of the title's data entering the repository.

Three examples take a folder or files of containers: `cargo run --release -p riptide-shader --example coverage -- <folder> [--compile] [--list-failures]` prints how many programs lift and emit, by stage, and the most frequent reasons for the rest, as counts and instruction and operation names only; with `--compile` it also compiles every emitted shader with the DirectX shader compiler, on every processor, and counts what the compiler refused. `--example lift -- <files>` prints the intermediate form of each program and `--example hlsl -- <files>` its HLSL; both show what the program does and so must stay on the machine.

## Depends on

Nothing in this workspace.

## Status

All four stages are built: container reading and instruction decoding (issue #18), stage data, the intermediate form, the lift, the reference interpreter, and the HLSL emitter. A second emitter that writes DXIL directly (design decision T3) is not built.

### Measured on the real title

The 8,634 shader containers the title hands to pipeline creation were read on the development machine (they are not in the repository). Every one parses and every program decodes. Lifted, emitted and compiled (by `dxc` 1.8.2502, with `ARGUMENTS`):

| Stage | Programs | Lifted | Emitted | Compiled |
| --- | --- | --- | --- | --- |
| Vertex | 1,034 | 1,017 (98.4%) | 1,017 | 1,017 |
| Pixel | 3,297 | 2,851 (86.5%) | 2,851 | 2,851 |
| Compute | 2,734 | 811 (29.7%) | 800 | 800 |
| Mesh | 1,569 | 0 | 0 | 0 |
| All | 8,634 | 4,679 (54.2%) | 4,668 (54.1%) | 4,668 |

Every lifted vertex and pixel program is emitted, and every emitted shader compiles. No program was written as a dispatch loop. The 11 lifted compute programs that are not emitted read across a quad (7) or read a lane by a constant number (4), which need the console's own lane numbers. Compiling all 4,668 took about five and a half minutes on 16 threads, a little over a second of one processor for the average shader; the compiler's time goes up steeply with the number of branches in a program, which is why the helpers that are called often are written without any.

The most frequent first reasons a program is not lifted: mesh programs as a whole (1,569); lane permutations within eight lanes and across rows, which compilers use for wave reductions (about 950, compute and pixel); atomic buffer and image operations (about 355); group-shared memory and barriers (about 380, compute); 16-bit float interpolation (121, pixel); a count of lanes in a mask held as a number (103, compute); lane swizzles across quads (63); compute programs whose parameters are spilled to memory (52); a register that holds a lane mask on one path and a number on another (about 100).

### What the lift covers

- The scalar formats: moves, selects, bitwise operations on numbers and on lane masks, the `saveexec` and `wrexec` forms, `s_wqm`, arithmetic with its condition codes, shifts of 32 and 64 bits, bit-field extracts, comparisons, bit counts and first-bit searches (on masks they become cross-lane operations), `s_getpc_b64` for far jumps and tables after the code, the fetch call of a vertex program, and writes to the float mode register (the rounding of 16-bit floats is followed; the scratch memory base is ignored).
- The vector formats: about 110 operations on 32-bit words, every comparison of 32-bit floats and integers and equality of 64-bit integers, `v_cndmask_b32`, carries, the absolute-value, negation, clamp and output-scale modifiers, the sub-register forms, lane permutations within a quad, `v_readfirstlane_b32`, `v_readlane_b32`, `v_writelane_b32` to a constant lane (and, where a vector register is used for nothing but such writes and reads at constant lanes, the scalar registers a compiler parked there as variables of their own: `lift/park.rs`), `v_mbcnt`, the mixed 16-bit and 32-bit multiply-adds, and 32-bit attribute interpolation.
- Memory: scalar loads by address and through a descriptor, buffer loads and stores of 8, 16 and 32 bits, typed buffer loads and stores with the format in the instruction or in the descriptor, loads and stores by address, and the quad lane swizzle.
- Images: every sampling and gathering variant by its suffixes, loads, stores, size and level queries, with 32-bit or 16-bit addresses.
- Exports of positions, parameters, colours and depth, and discard by the live mask at the final export.

### What the emitter covers

- Every operation of the intermediate form, in vertex, pixel and compute programs, except those listed under "What is not covered".
- Control flow as structured code (`structure.rs`): scopes, loops with `continue`, and jumps out of several scopes at once through one flag variable. One count limits the trips of all loops of an invocation (4,096; 65,536 in a compute program), because unmirrored data can make a console loop run without end.
- Title memory through the page table, at any address and alignment, across pages, with unmapped memory reading zero and dropping writes; buffers through their descriptors with the four bound checks; typed buffers in every format `ir::eval` knows.
- Images of every dimension: samples in all their variants, gathers, loads, stores, size and level queries, with integer formats read through integer texture types chosen at run time by the format word, the view of nothing for a record that names no texture, and samplers from the heap or fixed by the root signature.
- Vertex inputs declared from the input signature, each four wide so that the input assembler fills in what the title's vertex layout leaves out; clip and cull distances from the output signature.
- Pixel rows declared by how the program reads them: interpolated with the modifier of the weights used; `nointerpolation` and read as it is for a row only read at its first vertex; `nointerpolation` with `GetAttributeAtVertex` for the other per-vertex terms; and, where a program reads a row both ways or with both kinds of weights, weighed in the shader from the three vertices' values and the host's barycentric coordinates.
- Cross-lane operations as wave intrinsics, with a helper lane of a pixel shader answering for itself alone (section 7 of the research note); quad operations as quad reads in pixel shaders.

### Where the emitter is not exact

What a shader computes is what `ir::eval` defines, bit for bit, wherever the tests below can show it. The exceptions, all decided in the source where they are made:

- **The host's own functions.** A fused multiply-add is HLSL's `mad`, which the host may round once or twice. The reciprocal is a division. The reciprocal square root, square root, power of two, logarithm, sine and cosine are the host's, to the host's accuracy; sine and cosine keep the console's exact quarter turns and its rule outside -256..256. On the software adapter the logarithm does not take a denormal number for what it is. Which NaN an operation gives is the host's choice.
- **One over w.** The console hands a pixel program 1/w; the host hands over w, so the shader divides.
- **The ancillary word** carries only the sample number (bits 8 to 11); a program that reads it runs per sample. The fixed-point position is the pixel's whole coordinates.
- **Lane numbers.** The host numbers the lanes of a wave its own way. Operations that only pass lane numbers between each other (a ballot and a count of it, a first lane's number and a read of that lane) are right in any numbering and are emitted. A lane number written into the code is refused, except a constant mask that repeats every four lanes in a pixel program, which is a fact about the lane's place in its quad. (Scalars parked in lanes never reach the emitter: the lift turns them into variables.) Wave operations in a vertex shader see whatever lanes the host runs together.
- **Images.** A texel offset that is not a constant between -8 and 7 is added to the coordinates as a fraction of the size of level 0. Outside a pixel shader a sample at an implicit level is taken at level 0 (or at its bias). A comparing sample at an explicit level or with gradients compares at level 0, and one with a bias drops the bias. A sample of an integer format reads the nearest texel, wrapping. A gather at an explicit level is four loads clamped to the edge. Coordinates in texels are divided by the size of level 0. A cube is sampled by the direction worked back from its face coordinates; an array of cubes is read as one cube, and a cube's size query says six slices. A store writes all four channels.
- **Typed buffer stores** of normalised formats round the scaled value in single precision where `ir::eval` uses double, and the formats with 10-bit and 11-bit floats are not written.
- **Stores of less than a word** are two interlocked operations on the word, so that lanes writing other bytes of it do not undo each other; they are not one atomic step.
- **A loop cut short** by the trip limit ends the lane where it is.

What a shader asks of the host device beyond shader model 6.6 with both heaps indexed directly: wave operations in whatever stage the program has them (609 of the title's 2,851 emitted pixel shaders and 260 of its 1,017 vertex shaders); and, for the pixel shaders that read per-vertex values other than the first vertex's (151) or the weights as numbers (254 declare `SV_Barycentrics`), the host's barycentrics feature.

Two things in the binding contract that the emitter had to decide for itself:

- **A load from a view for unordered access.** A texture's record names one place in the heap, and a host view for unordered access must be read through a different HLSL type than one for reading. The image operation carries only words 5 and 6 of the record, which do not say which the view is. The emitter reads bit 2 of word 6 as "the view is for unordered access" and then loads (and asks the size) through the type it would store through. `riptide-gpu` does not set that bit yet, so such loads read through the wrong type until it does.
- **Parameter numbers.** Row n is the vertex program's parameter n and the pixel program's attribute n, as the design says. Nothing in the emitter checks that the two numberings agree for a given pair of programs.

### What is not covered

- Mesh programs, the hull and geometry stages and the stages merged with them, and compute programs whose parameters are spilled to memory.
- In the emitter: a read of a lane by a constant number and the lane's bit of a constant mask (where they are not a parked scalar or a quad pattern, see above), and quad operations outside a pixel shader, which need the console's lane numbers; a gather at a level from derivatives; a gather of an image that is not flat; a comparing sample of a volume; a sample or a store of a multisampled image; more than eleven samplers fixed by one root signature; user-data words past the sixty-fourth. Each is an `EmitError` that names the operation.
- Wave reductions: lane permutations within eight lanes and across rows (`v_permlanex16_b32`) are refused, not recognised as reductions.
- Group-shared memory, barriers, atomics, scratch memory, 64-bit floats, most 16-bit arithmetic, packed 16-bit operations, 16-bit interpolation, `v_cvt_pk_u8_f32`, ray intersection.
- A scalar register that holds a lane mask on one path into a join and a number on another, read after the join, fails the program. The other project retries such programs in a second mode.
- The contents of the compiler options blob, of the `PRIV` part, and of the two blob kinds whose meaning is unknown; the dependency tables at the end of `PSV0`; the `ACCC` part.
- Register jumps other than the compilers' far-jump sequence are not followed.

### What is assumed rather than checked

The doc comments of each module say this in place. In summary:

- The layouts of the console's additions (`XDAT` table, `XBCH`, `FSSM`, stage data) were worked out from shipped titles by other projects. The stage data offsets were checked against the real title by counts: tables agree with the signatures, the patch count equals the number of colour targets, the register counts agree with the root parameter tables. The meaning of the flag bits and usage codes is taken on trust.
- That a lane may branch on its own bit where the console branches the whole wave is sound only for code a compiler made. It is taken from the research note and tested on the patterns compilers emit.
- The float rules, the buffer bound checks, the data format numbers and the 16-bit image address packing are from AMD's public descriptions and the research note. Nothing was measured on console hardware. Where the hardware is approximate (reciprocal, square roots, exponentials, sine and cosine) the reference gives the correctly rounded result.
- A vertex program's vertex number in `v0` and instance number in `v3`, and that a fetch routine changes no register but the attributes', are taken on trust.
- The printed instruction text follows LLVM's syntax from knowledge of it and has not been compared mechanically with LLVM's output.
- The emitter's bindings are the design's, proven by `riptide-gpu`'s own tests for the page table, the banks, a texture and a sampler. That a pixel position's fourth component is w on the host, that vertex 0 of `GetAttributeAtVertex` is the console's provoking vertex, and that the second and third barycentric coordinates are the console's i and j are taken from the host's documentation, not measured.

### How it is tested

The tests use synthetic containers and hand-assembled instructions only. The lift's tests assemble programs that use each lifted operation and each control-flow pattern (if, else, loops, a loop over the first lane's value, early outs, a loop entered in its middle), run them through the lift and the reference interpreter and through the machine interpreter on 64 lanes with inputs that differ per lane (NaNs, infinities and both zeroes among them), and compare every export, store and discard. The simple arithmetic is written twice, once in each interpreter; the float rules that need care are shared between them and tested on their own in `ir::eval`.

The emitter is tested in three ways, none with game data:

- **Structure.** Six hundred random graphs are made reducible and structured, and the structured code is walked against the graph with random branch outcomes: both must visit the same blocks in the same order. A graph that cannot be structured must come out as a dispatch loop and say so.
- **Text and compilation.** Tests check what a shader declares and binds for each stage (rows and their modifiers, inputs by signature, targets by format class, samplers and their input words, group sizes, the limits on loops) and that each refused operation is refused by name. Where the DirectX shader compiler is installed (looked for where `riptide-gpu` looks), a shader with every arithmetic operation, one with every memory, buffer and format operation, one with every image operation on every dimension (as a pixel shader and as a compute shader), each cross-lane operation in both stages, the stage interfaces, and the rectangle shader are compiled; without a compiler those checks are skipped.
- **On a device.** `cargo test -p riptide-shader -- --ignored on_a_device` runs emitted compute shaders on the software adapter (WARP) against a title memory bound exactly as the contract says (page table, bank buffers in the heap, stage inputs), and compares the memory afterwards with what `interp::run_wave` leaves, word for word: every unary, binary, ternary and comparison operation, the float classes, carries and flag logic on 3,072 operand triples that include every pair of 40 words chosen to need care (both zeroes, infinities, NaNs, denormals, the edges of the conversions); loads and stores of every width at odd addresses, across pages, across 4 GiB and into unmapped memory; buffer loads and stores through descriptors with every bound check; typed loads in all 77 formats and typed stores in every format that is written; and sixty random control-flow graphs, at least five of which need the flag for a jump out of several scopes. The comparison is exact except for the host's own functions listed above, which are compared to a tolerance.

What the tests do not show: anything about images on a device (they are compiled, not run); cross-lane and quad operations on a device (the software adapter's waves are not the console's); the pixel and vertex stage interfaces on a device (interpolation, per-vertex values, the rectangle shader); and any behaviour of a hardware driver.
