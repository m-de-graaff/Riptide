# riptide-shader

Console shader code (RDNA 2) to host shaders, including decoding resource descriptors inside the shader.

## Interface

The goal of the crate is to translate one shader from its bytes to a host shader plus a description of what it reads, keyed by content hash, knowing nothing of D3D12 objects. The design is `docs/design/2026-11-28-shader-translation.md`. Three of its four stages exist: reading the container, decoding the machine code, and lifting it into an intermediate form, with a reference interpreter for that form. The emitter of host shaders is not built. Everything is a pure function of its input, with no GPU or OS dependencies, no other crates and no unsafe code. Malformed input gives an error, never a panic.

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

`riptide_shader::summarize(container_bytes)` reads a container, decodes every program in it and returns a `Summary` of counts, which the runtime can log for every shader of a real title without any of the title's data entering the repository.

Two examples take a folder or files of containers: `cargo run --release -p riptide-shader --example coverage -- <folder>` prints how many programs lift, by stage, and the most frequent reasons for the rest, as counts and instruction names only; `--example lift -- <files>` prints the intermediate form of each program, which shows what the program does and so must stay on the machine.

## Depends on

Nothing in this workspace.

## Status

Stages one to three of four are built: container reading and instruction decoding (issue #18), stage data, the intermediate form, the lift and the reference interpreter. No host shader is produced yet.

### Measured on the real title

The 8,634 shader containers the title hands to pipeline creation were read on the development machine (they are not in the repository). Every one parses and every program decodes. The lift covers:

| Stage | Programs | Lifted |
| --- | --- | --- |
| Vertex | 1,034 | 1,017 (98.4%) |
| Pixel | 3,297 | 2,851 (86.5%) |
| Compute | 2,734 | 811 (29.7%) |
| Mesh | 1,569 | 0 |
| All | 8,634 | 4,679 (54.2%) |

The most frequent first reasons a program is not lifted: mesh programs as a whole (1,569); lane permutations within eight lanes and across rows, which compilers use for wave reductions (about 950, compute and pixel); atomic buffer and image operations (about 355); group-shared memory and barriers (about 380, compute); 16-bit float interpolation (121, pixel); a count of lanes in a mask held as a number (103, compute); lane swizzles across quads (63); compute programs whose parameters are spilled to memory (52); a register that holds a lane mask on one path and a number on another (about 100).

### What the lift covers

- The scalar formats: moves, selects, bitwise operations on numbers and on lane masks, the `saveexec` and `wrexec` forms, `s_wqm`, arithmetic with its condition codes, shifts of 32 and 64 bits, bit-field extracts, comparisons, bit counts and first-bit searches (on masks they become cross-lane operations), `s_getpc_b64` for far jumps and tables after the code, the fetch call of a vertex program, and writes to the float mode register (the rounding of 16-bit floats is followed; the scratch memory base is ignored).
- The vector formats: about 110 operations on 32-bit words, every comparison of 32-bit floats and integers and equality of 64-bit integers, `v_cndmask_b32`, carries, the absolute-value, negation, clamp and output-scale modifiers, the sub-register forms, lane permutations within a quad, `v_readfirstlane_b32`, `v_readlane_b32`, `v_writelane_b32` to a constant lane, `v_mbcnt`, the mixed 16-bit and 32-bit multiply-adds, and 32-bit attribute interpolation.
- Memory: scalar loads by address and through a descriptor, buffer loads and stores of 8, 16 and 32 bits, typed buffer loads and stores with the format in the instruction or in the descriptor, loads and stores by address, and the quad lane swizzle.
- Images: every sampling and gathering variant by its suffixes, loads, stores, size and level queries, with 32-bit or 16-bit addresses.
- Exports of positions, parameters, colours and depth, and discard by the live mask at the final export.

### What is not covered

- Mesh programs, the hull and geometry stages and the stages merged with them, and compute programs whose parameters are spilled to memory.
- Wave reductions: lane permutations within eight lanes, across rows (`v_permlanex16_b32`), and `v_readlane_b32` of registers a compiler parks scalar registers in are lifted literally or refused, not recognised as reductions or as parking.
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

### How it is tested

The tests use synthetic containers and hand-assembled instructions only. The lift's tests assemble programs that use each lifted operation and each control-flow pattern (if, else, loops, a loop over the first lane's value, early outs, a loop entered in its middle), run them through the lift and the reference interpreter and through the machine interpreter on 64 lanes with inputs that differ per lane (NaNs, infinities and both zeroes among them), and compare every export, store and discard. The simple arithmetic is written twice, once in each interpreter; the float rules that need care are shared between them and tested on their own in `ir::eval`.
