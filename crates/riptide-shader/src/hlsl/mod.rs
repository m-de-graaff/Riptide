//! The emitter: from the intermediate form to HLSL for the DirectX shader
//! compiler, shader model 6.6.
//!
//! [`translate`] takes a container's bytes and a hardware stage and returns
//! a [`Shader`]: complete HLSL with the entry point `main`, the profile to
//! compile it for, and what the host has to know to bind it. The shader is
//! written against the bindings of
//! `docs/design/2026-11-28-shader-translation.md` ("The host shader's
//! bindings", "What passes between stages"), which `riptide-gpu` provides:
//!
//! - The stage's inputs are 80 words in a constant buffer at `b0`, in space
//!   0 for a vertex program, 1 for a pixel program and 2 for a compute
//!   program. Word n below 64 is user-data word n. Words 67 to 77 are the
//!   places in the sampler heap of the samplers the root signature fixes
//!   ([`Shader::samplers`], in that order), word 78 is the draw's first
//!   instance and word 79 its first vertex.
//! - Title memory is read through the page table at `t0`, space 0, and the
//!   banks at places 0 to 15 of the resource heap (16 to 31 to write).
//! - A texture is at the place in the resource heap that its descriptor
//!   names, and a sampler at its place in the sampler heap, with the
//!   comparing form 1,024 places on.
//! - Parameter n passes from a vertex shader to a pixel shader as
//!   `float4 ROWn`, and the position is `SV_Position` after the rows.
//! - The group-shared memory of a compute program is the shader's own
//!   `groupshared` array of words, of the size the program's stage data
//!   names; nothing is bound for it.
//!
//! [`rectangle_geometry_shader`] writes the geometry shader that completes
//! a console rectangle from its three vertices.
//!
//! # Exactness
//!
//! Every operation computes what [`crate::ir::eval`] defines, bit for bit,
//! except where the host has no way to: [`helpers`] holds the HLSL of each,
//! and the crate's README lists the approximations. Compile with
//! [`ARGUMENTS`]: the helpers already declare their float arithmetic
//! `precise`, and the arguments ask the compiler for strict IEEE rules
//! everywhere else and for denormal numbers to be kept.

mod emit;
mod helpers;
mod image;
mod structure;

#[cfg(all(test, windows))]
mod device;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

use crate::container::Container;
use crate::container::xbox::{HardwareStage, RootParameter};
use crate::lift::{LiftError, lift};

pub use emit::Emitted;
use emit::{Signatures, emit};

/// The version of the emitter. It changes whenever the text emitted for the
/// same program could change, so that a cache of translations keyed by it
/// never serves a stale one.
pub const VERSION: &str = "2";

/// The arguments to give the DirectX shader compiler besides the profile
/// and the entry point: the 2021 language, strict IEEE float rules, and
/// denormal numbers kept as the console keeps them.
pub const ARGUMENTS: &[&str] = &["-HV", "2021", "-Gis", "-denorm", "preserve"];

/// How a render target's format takes the four words a pixel program
/// exports to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum TargetKind {
    /// A float or normalised format: the words are the bits of four floats.
    #[default]
    Float,
    /// An unsigned integer format.
    Unsigned,
    /// A signed integer format.
    Signed,
}

/// What a translation is made for besides the program itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Options {
    /// The format class of each render target a pixel program writes.
    pub targets: [TargetKind; 8],
}

/// A translated program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shader {
    /// Complete HLSL. The entry point is `main`.
    pub source: String,
    /// The profile to compile for: `vs_6_6`, `ps_6_6` or `cs_6_6`.
    pub profile: &'static str,
    /// The hardware stage of the program.
    pub stage: HardwareStage,
    /// The program's root parameters, which say what each user-data word
    /// holds.
    pub root_parameters: Vec<RootParameter>,
    /// The samplers the root signature fixes, as their four hardware words.
    /// The shader takes the k-th one's place in the sampler heap from stage
    /// input word 67 + k.
    pub samplers: Vec<[u32; 4]>,
    /// A vertex shader: how many rows it puts out. A pixel shader: how many
    /// it declares.
    pub rows: u32,
    /// Bit n set: row n carries bits that must not be interpolated. Known
    /// for a pixel shader; a vertex shader does not know and says 0.
    pub flat_rows: u32,
    /// Whether the control flow could not be structured and was written as
    /// a loop around a `switch` instead. The lift only hands out graphs
    /// that can be structured, so this is false for every lifted program;
    /// it is here to be counted.
    pub dispatched: bool,
}

/// An operation the emitter has no HLSL for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EmitError {
    /// The operation of the intermediate form, by its name there.
    pub op: &'static str,
    /// Why, in words that hold nothing of the program itself.
    pub reason: &'static str,
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} has no HLSL form: {}", self.op, self.reason)
    }
}

impl std::error::Error for EmitError {}

/// Why a container was not translated: the step that failed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateError {
    /// The container could not be read.
    Read(crate::Error),
    /// The container holds no program of the stage asked for.
    NoProgram(HardwareStage),
    /// The machine code or the stage's data could not be unpacked.
    Decode(crate::Error),
    /// An instruction could not be lifted.
    Lift(LiftError),
    /// An operation could not be written as HLSL.
    Emit(EmitError),
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranslateError::Read(error) => write!(f, "read: {error}"),
            TranslateError::NoProgram(stage) => {
                write!(
                    f,
                    "read: no {} program in the container",
                    stage.short_name()
                )
            }
            TranslateError::Decode(error) => write!(f, "decode: {error}"),
            TranslateError::Lift(error) => write!(f, "lift: {error}"),
            TranslateError::Emit(error) => write!(f, "emit: {error}"),
        }
    }
}

impl std::error::Error for TranslateError {}

/// Writes the HLSL of one lifted function: the emitter alone, for callers
/// that have lifted a program themselves. `inputs` and `outputs` are the
/// container's signatures: a vertex shader's inputs are declared with the
/// names and types of the first, and its clip and cull distances are those
/// the second declares.
pub fn emit_function(
    function: &crate::ir::Function,
    inputs: Option<&crate::container::signature::Signature>,
    outputs: Option<&crate::container::signature::Signature>,
    options: &Options,
) -> Result<Emitted, EmitError> {
    emit(function, Signatures { inputs, outputs }, options)
}

/// Translates the program of one hardware stage in a container to HLSL.
pub fn translate(
    container: &[u8],
    stage: HardwareStage,
    options: &Options,
) -> Result<Shader, TranslateError> {
    let profile = match stage {
        HardwareStage::Vertex => "vs_6_6",
        HardwareStage::Pixel => "ps_6_6",
        HardwareStage::Compute => "cs_6_6",
        _ => {
            return Err(TranslateError::Emit(EmitError {
                op: "the function",
                reason: "only vertex, pixel and compute programs have a host shader",
            }));
        }
    };
    let container = Container::parse(container).map_err(TranslateError::Read)?;
    let inputs = container.input_signature().map_err(TranslateError::Read)?;
    let outputs = container.output_signature().map_err(TranslateError::Read)?;
    let dxil = container
        .xbox_dxil()
        .map_err(TranslateError::Read)?
        .ok_or(TranslateError::Read(crate::Error::NoMachineCode))?;
    let code = dxil.machine_code().map_err(TranslateError::Decode)?;
    let (machine, program) = code
        .iter()
        .find_map(|machine| {
            machine
                .programs
                .iter()
                .find(|program| program.stage == stage)
                .map(|program| (machine, program))
        })
        .ok_or(TranslateError::NoProgram(stage))?;
    let data = machine
        .stage_data(program)
        .map_err(TranslateError::Decode)?;
    let function = lift(program, &data).map_err(TranslateError::Lift)?;
    let emitted = emit_function(&function, inputs.as_ref(), outputs.as_ref(), options)
        .map_err(TranslateError::Emit)?;
    Ok(Shader {
        source: emitted.source,
        profile,
        stage,
        root_parameters: program.root_parameters.clone(),
        samplers: emitted.samplers,
        rows: emitted.rows,
        flat_rows: emitted.flat_rows,
        dispatched: emitted.dispatched,
    })
}

/// Writes the geometry shader that makes a console rectangle of the three
/// vertices the title draws for it (the design's "Rectangles").
///
/// It takes a triangle of a vertex shader's output: `rows` rows and the
/// position. If one vertex shares x with one of the others and y with the
/// other, the three are corners of a screen-aligned rectangle; the fourth
/// corner lies opposite that vertex, at `a + b - corner` for the position
/// and for every row, and the shader puts out the four as a strip. Any
/// other three vertices go out as the triangle they are.
///
/// Bit n of `flat` says row n carries bits that are not floats to continue
/// ([`Shader::flat_rows`] of the pixel shader that reads them): such a row
/// is copied from the triangle's first vertex. The entry point is `main`
/// and the profile `gs_6_6`.
pub fn rectangle_geometry_shader(rows: u32, flat: u32) -> String {
    use std::fmt::Write as _;
    let rows = rows.min(32);
    let mut text = String::from(
        "// Written by riptide-shader: completes a rectangle from three of its corners.\nstruct Vertex {\n",
    );
    for row in 0..rows {
        let _ = writeln!(text, "    float4 row{row} : ROW{row};");
    }
    text.push_str("    float4 position : SV_Position;\n};\n\n");
    text.push_str(
        "\
// The fourth corner, opposite `corner`.
Vertex fourth(Vertex corner, Vertex a, Vertex b, Vertex first) {
    Vertex made;
    made.position = a.position + b.position - corner.position;
",
    );
    for row in 0..rows {
        if flat >> row & 1 != 0 {
            let _ = writeln!(text, "    made.row{row} = first.row{row};");
        } else {
            let _ = writeln!(
                text,
                "    made.row{row} = a.row{row} + b.row{row} - corner.row{row};"
            );
        }
    }
    text.push_str(
        "\
    return made;
}

[maxvertexcount(4)]
void main(triangle Vertex input[3], inout TriangleStream<Vertex> stream) {
    // Where each vertex lies on the screen.
    float2 at[3];
    for (uint each = 0u; each < 3u; each++) at[each] = input[each].position.xy / input[each].position.w;
    // The corner between the other two shares x with one and y with the other.
    uint corner = 3u;
    for (uint which = 0u; which < 3u; which++) {
        uint next = (which + 1u) % 3u, last = (which + 2u) % 3u;
        bool between = (at[which].x == at[next].x && at[which].y == at[last].y)
            || (at[which].x == at[last].x && at[which].y == at[next].y);
        if (between && corner == 3u) corner = which;
    }
    if (corner == 3u) {
        stream.Append(input[0]);
        stream.Append(input[1]);
        stream.Append(input[2]);
        return;
    }
    // The strip keeps the triangle's winding: corner, the vertex after it,
    // the vertex before it, then the corner made.
    uint next = (corner + 1u) % 3u, last = (corner + 2u) % 3u;
    stream.Append(input[corner]);
    stream.Append(input[next]);
    stream.Append(input[last]);
    stream.Append(fourth(input[corner], input[next], input[last], input[0]));
}
",
    );
    text
}
