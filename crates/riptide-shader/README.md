# riptide-shader

Console shader code (RDNA 2) to DXIL, including decoding resource descriptors inside the shader.

## Interface

The goal of the crate is to translate one shader from its bytes to a DXIL blob plus a description of what it reads, keyed by content hash, keeping the console's wave size and knowing nothing of D3D12 objects. What exists so far is the first stage: reading a shader's container and decoding its machine code. Everything is a pure function of the input bytes, with no GPU or OS dependencies, no other crates and no unsafe code. Malformed input gives an `Error`, never a panic.

`riptide_shader::container` reads the DXBC container:

- `Container::parse` splits a container into its parts by four-letter name (`FourCc`, `Part`).
- `Container::input_signature`, `output_signature` and `patch_signature` read `ISG1`, `OSG1` and `PSG1` into a `Signature` of `SignatureElement`s: semantic name and index, register, component mask, component type, system value.
- `Container::pipeline_validation` reads `PSV0` into a `PipelineValidation`: the kind of shader, the facts particular to its stage (`stage_details`), the wave lane counts, the thread group size, the entry point name, the resource bindings (type, register space, first and last register, resource kind, flags) and the signature rows.
- `Container::xbox_dxil` reads the `DXIL` part into an `XboxDxil`: the program header, and on the console the table of blobs after it, each `Blob` with its `BlobKind`. `XboxDxil::compiler_options` returns the compiler options blob as bytes.
- `XboxDxil::machine_code` finds the machine code blobs, unpacks the packed ones, and returns each as a `MachineCode` holding one `Program` per hardware stage: the stage, the wave size, the register counts, the root parameters and used resources, and the code as 32-bit words.
- `Container::feature_flags` and `shader_hash` read `SFI0` and `HASH`.

`riptide_shader::inflate` unpacks zlib streams (`zlib`, `deflate_raw`, `adler32`). The machine code is stored as one, and the workspace takes no dependency for it.

`riptide_shader::gfx10` decodes the machine code of the GPU generation called GFX10:

- `decode_at(code, index, wave)` decodes the instruction at one word into an `Instruction`: its `Format`, opcode number, mnemonic, words, operands (`Arg`: a `Role` and an `Operand`) and the other fields of its format (`Detail`). It never fails. An instruction it cannot name keeps its words and has no mnemonic.
- `walk(code, wave)` decodes a program by following its control flow from the first word, because compilers put data after the end of a program. `decode_linear` decodes word after word.
- `Instruction` implements `Display` in the syntax of LLVM's assembler, and `listing` prints a list of instructions with offsets.

`riptide_shader::summarize(container_bytes)` reads a container, decodes every program in it and returns a `Summary` of counts: the shader stage, the number of code words, of instructions decoded and of instructions that could not be named (with the first few of those), and how often each mnemonic occurs. The runtime can log this for every shader of a real title, which is how the decoder is checked without any of the title's data entering the repository.

## Depends on

Nothing in this workspace.

## Status

The first stage is built (issue #18): container reading and instruction decoding. Translation is not started; nothing here gives an instruction a meaning or produces a host shader.

What is covered:

- The DXBC container, the three signature parts, `PSV0` up to and including its signature rows, `SFI0`, `HASH`, and the console's `DXIL` part: program header, blob table, the packed wrapper (`XBCH`, zlib) and the machine code structure (`FSSM`) with each stage's record, root parameter table, used-resource table and code.
- Inflate for all three DEFLATE block types, with the Adler-32 check and a limit on the output size.
- Every instruction format of the generation: SOP2, SOPK, SOP1, SOPC, SOPP, SMEM, VOP2, VOP1, VOPC, VOP3A, VOP3B, VOP3P, VINTRP, DS, FLAT, GLOBAL, SCRATCH, MUBUF, MTBUF, MIMG and EXP, with the sub-register (SDWA) and lane permutation (DPP16, DPP8) forms of vector operations, literal constants, and the extra address words of image instructions. The opcode tables hold 1232 opcodes, the GFX10 column of Mesa's `aco_opcodes.py`.

What is not covered:

- The stage data that follows each stage's record in the machine code structure: the pixel shader's interpolated inputs and the code patches for its colour outputs, the vertex shader's attributes and outputs, the compute shader's thread group size, the mesh shader's limits. `MachineCode::payload` and `Program::record_offset` are kept so that the next stage can read it. A pixel shader's export instructions are decoded as stored, before any such patch is applied.
- The contents of the compiler options blob, of the `PRIV` part, and of the two blob kinds whose meaning is unknown. They are available as bytes.
- The dependency tables at the end of `PSV0`.
- The `ACCC` part that a container carries in place of `DXIL` when another compiler made the machine code.
- Register jumps other than the compilers' far-jump sequence are not followed by `walk`; code reachable only through one is counted as not reached.

What is assumed rather than checked, and said so in the doc comments:

- The layouts of the console's additions (`XDAT` table, `XBCH`, `FSSM`) were worked out from shipped titles by other projects, not taken from documentation. The third word of the table header has only been seen as 0x24 and is taken to be the offset of the first entry. Structures with a version other than the one seen are rejected.
- The signature and `PSV0` layouts follow the public DirectX shader compiler's headers and have not yet been compared with a real console shader.
- The width in registers of each operand, and the number of sources of each operation, are derived from the operation's mnemonic with listed exceptions, not stored per opcode.
- The printed text follows LLVM's syntax from knowledge of it and has not been compared mechanically with LLVM's output. The modifiers of sub-register, lane permutation, typed buffer and packed operations are the likeliest to differ in detail.

None of this has been run on a real shader yet. The tests use synthetic containers and hand-encoded instructions only.
