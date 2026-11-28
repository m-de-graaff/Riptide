//! The data that follows a stage's record in the machine code structure.
//!
//! After the 0x54 bytes of a stage's record ([`super::xbox`]) comes data
//! particular to the stage: what a pixel program takes in and how its colour
//! exports are patched, a vertex program's attributes and outputs, a compute
//! program's thread group. The layouts were worked out from shipped titles by
//! another project (`docs/research/2026-11-28-console-shader-convention.md`,
//! section 5). Offsets below count from the end of the record.
//!
//! ```text
//! pixel
//!   0x024  32 input entries of 8 bytes: usage, signature row, component
//!          mask, attribute number, default, second row, parameter slot, flags
//!   0x124  number of inputs, u32
//!   0x134  SPI_PS_INPUT_ADDR, u32: which system inputs arrive in registers
//!   0x138  DB_SHADER_CONTROL, u32
//!   0x148  size of an export patch in 32-bit words, u32
//!   0x14C  8 patches of 404 bytes, one per colour target: the code word the
//!          patch goes to (0xFFFFFFFF for none), then 10 variants of 10 words
//! vertex
//!   0x024  32 input entries of 5 bytes: usage, signature row, component
//!          mask, vector register of the first component, highest component
//!   0x0C4  number of inputs, u32
//!   0x0C8  32 output entries of 6 bytes: usage, signature row, parameter
//!          number, row again, slot, flags
//!   0x188  number of outputs, u32
//! compute
//!   0x024  COMPUTE_PGM_RSRC2, u32
//!   0x028  thread group size, 3 x u32
//! mesh
//!   0x0C8  outputs, as for vertex
//!   0x188  number of outputs, u32
//!   0x18C  eight hardware register values, u32 each
//!   0x448  lanes per thread group u16, most vertices u16, most primitives
//!          u16, user-data registers u8, topology u8
//! ```
//!
//! What was checked against the 8,634 programs of the real title (by counts
//! only; see the README):
//!
//! - Pixel: every input entry names a row of `ISG1` with an overlapping
//!   component mask; the attribute number of entry n is n; the input count
//!   is never above the number of signature elements; the patch size is 6
//!   words in every program; the number of patches with a code word equals
//!   the number of `SV_Target` rows of `OSG1` in every program; every patch
//!   lies inside the code; the stored code holds variant 4 at every one of
//!   the 3,774 patch sites (a conversion to 16-bit floats and a packed
//!   export), so a program is never stored "unpatched"; variant 9 is four
//!   `s_nop` and an `exp` of the four registers themselves;
//!   `DB_SHADER_CONTROL` only has bits the public register has.
//! - Vertex: every input and output entry names a row of its signature; a
//!   program has inputs exactly when it calls a fetch routine with
//!   `s_swappc_b64` (332 of 1,034); first-component registers are 4, 8, 12
//!   and 16.
//! - Compute: the user-data register count of `COMPUTE_PGM_RSRC2` equals the
//!   count the root parameter table needs in all but the programs whose
//!   parameters are spilled to memory; the group sizes are small.
//! - Mesh: the lane count is 128 and the topology 2 in every program, and
//!   the user-data register count equals the root parameter table's.
//!
//! What is taken on trust: the meaning of the flag bits, of the usage codes
//! and of the `default` byte (always 0 in the title), and everything about
//! the stages the title does not use (hull, geometry and the merged stages),
//! whose data is not read.

use super::xbox::{HardwareStage, MachineCode, Program};
use crate::Error;
use crate::bytes::{span, u8_at, u16_at, u32_at};

/// The size of a stage record, after which the stage data starts.
const RECORD_SIZE: u64 = 0x54;
/// How many entries each input and output table has room for.
const TABLE_ROOM: usize = 32;
/// How many colour targets a pixel program can have a patch for.
const PATCH_TARGETS: usize = 8;
/// How many variants each patch has.
const PATCH_VARIANTS: usize = 10;
/// How many words each variant has room for.
const VARIANT_ROOM: usize = 10;
/// The size of one patch: the code word and the variants.
const PATCH_SIZE: usize = 4 + 4 * PATCH_VARIANTS * VARIANT_ROOM;
/// The code word a patch states when its target has no export.
const NO_PATCH: u32 = u32::MAX;

/// One input of a pixel program: where an interpolated attribute comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelInput {
    /// The usage code. Seen: 6, a generic attribute.
    pub usage: u8,
    /// The register (row) of `ISG1` the input is.
    pub row: u8,
    /// Which components of the row the input carries, bit 0 for x.
    pub mask: u8,
    /// The attribute number: the one interpolation instructions name.
    pub attribute: u8,
    /// The default value code. Seen: 0.
    pub default: u8,
    /// The second row packed into the same attribute, or 0.
    pub second_row: u8,
    /// The parameter slot of the stage before that feeds the attribute.
    pub slot: u8,
    /// The flag bits as stored; see the methods.
    pub flags: u8,
}

impl PixelInput {
    /// Whether the attribute is not interpolated (bit 0).
    pub fn is_flat(&self) -> bool {
        self.flags & 1 != 0
    }

    /// Whether the attribute holds 16-bit floats (bit 3).
    pub fn is_half(&self) -> bool {
        self.flags & 8 != 0
    }

    /// Whether two 16-bit values share each 32-bit word (bit 4).
    pub fn is_packed(&self) -> bool {
        self.flags & 0x10 != 0
    }
}

/// The form a pixel program's colour export is patched to. The console
/// writes the variant for the bound target's format when a pipeline is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    /// Nothing is exported.
    Zero = 0,
    /// One 32-bit value.
    R32 = 1,
    /// Two 32-bit values.
    Gr32 = 2,
    /// Two 32-bit values: alpha and red.
    Ar32 = 3,
    /// Four 16-bit floats.
    Half = 4,
    /// Four 16-bit unsigned normalised values.
    Unorm16 = 5,
    /// Four 16-bit signed normalised values.
    Snorm16 = 6,
    /// Four 16-bit unsigned integers.
    Uint16 = 7,
    /// Four 16-bit signed integers.
    Sint16 = 8,
    /// Four raw 32-bit values. The translator applies this one, and the host
    /// converts to the target's format.
    Abgr32 = 9,
}

/// The patch of one colour target's export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPatch {
    /// The colour target, 0 to 7.
    pub target: u8,
    /// The index of the code word the patch starts at.
    pub code_word: u32,
    /// The ten variants, by [`ExportFormat`] number. Each has room for ten
    /// words, of which a program uses [`PixelData::patch_words`].
    pub variants: [[u32; VARIANT_ROOM]; PATCH_VARIANTS],
}

/// The stage data of a pixel program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelData {
    /// The interpolated inputs. Entry n describes attribute n.
    pub inputs: Vec<PixelInput>,
    /// `SPI_PS_INPUT_ADDR`: which system inputs arrive in vector registers
    /// at the start of the program. See [`PixelData::system_inputs`].
    pub input_enable: u32,
    /// `DB_SHADER_CONTROL`: bit 0 says the program exports depth, bit 1 a
    /// stencil reference, bit 6 that it can discard, bit 8 that it exports a
    /// sample mask.
    pub depth_control: u32,
    /// How many words a patch replaces.
    pub patch_words: u32,
    /// The patches of the colour targets the program exports to.
    pub patches: Vec<ExportPatch>,
}

/// A system input of a pixel program that arrives in vector registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelSystemInput {
    /// Perspective-correct barycentric weights i and j at the sample.
    PerspectiveSample,
    /// The same at the pixel centre.
    PerspectiveCenter,
    /// The same at the centroid of the covered samples.
    PerspectiveCentroid,
    /// The pull model's three values, from which a program derives weights.
    PerspectivePullModel,
    /// Linear (not perspective-correct) weights at the sample.
    LinearSample,
    /// The same at the pixel centre.
    LinearCenter,
    /// The same at the centroid.
    LinearCentroid,
    /// The line stipple texture coordinate.
    LineStipple,
    /// The pixel's x position.
    PositionX,
    /// The pixel's y position.
    PositionY,
    /// The pixel's depth.
    PositionZ,
    /// One over the pixel's w.
    PositionW,
    /// Non-zero when the primitive faces front.
    FrontFace,
    /// A packed word: the sample number among its fields.
    Ancillary,
    /// The mask of covered samples.
    SampleCoverage,
    /// The pixel's position as two 16-bit integers.
    FixedPosition,
}

impl PixelSystemInput {
    /// The inputs in the order of the bits of `SPI_PS_INPUT_ADDR`, which is
    /// also the order their registers are handed out in.
    pub const ALL: [PixelSystemInput; 16] = [
        PixelSystemInput::PerspectiveSample,
        PixelSystemInput::PerspectiveCenter,
        PixelSystemInput::PerspectiveCentroid,
        PixelSystemInput::PerspectivePullModel,
        PixelSystemInput::LinearSample,
        PixelSystemInput::LinearCenter,
        PixelSystemInput::LinearCentroid,
        PixelSystemInput::LineStipple,
        PixelSystemInput::PositionX,
        PixelSystemInput::PositionY,
        PixelSystemInput::PositionZ,
        PixelSystemInput::PositionW,
        PixelSystemInput::FrontFace,
        PixelSystemInput::Ancillary,
        PixelSystemInput::SampleCoverage,
        PixelSystemInput::FixedPosition,
    ];

    /// How many vector registers the input takes.
    pub fn registers(self) -> u8 {
        match self {
            PixelSystemInput::PerspectiveSample
            | PixelSystemInput::PerspectiveCenter
            | PixelSystemInput::PerspectiveCentroid
            | PixelSystemInput::LinearSample
            | PixelSystemInput::LinearCenter
            | PixelSystemInput::LinearCentroid => 2,
            PixelSystemInput::PerspectivePullModel => 3,
            _ => 1,
        }
    }
}

impl PixelData {
    /// The system inputs the program is given, each with the first vector
    /// register it arrives in. Registers are handed out from `v0` up in bit
    /// order, only for the bits that are set.
    pub fn system_inputs(&self) -> Vec<(PixelSystemInput, u8)> {
        let mut out = Vec::new();
        let mut register = 0u8;
        for (bit, input) in PixelSystemInput::ALL.into_iter().enumerate() {
            if self.input_enable >> bit & 1 != 0 {
                out.push((input, register));
                register += input.registers();
            }
        }
        out
    }

    /// Returns `code` with every colour export patched to `format`.
    ///
    /// The stored code holds the variant for 16-bit float targets
    /// ([`ExportFormat::Half`]) at each patch site: it converts the colour
    /// and exports it packed. A program is patched to
    /// [`ExportFormat::Abgr32`] before it is decoded for translation, so
    /// that its exports are the four values it computed.
    pub fn patched_code(&self, code: &[u32], format: ExportFormat) -> Result<Vec<u32>, Error> {
        let mut out = code.to_vec();
        let words = self.patch_words as usize;
        if words > VARIANT_ROOM {
            return Err(Error::Malformed(
                "a colour export patch is longer than a variant has room for",
            ));
        }
        for patch in &self.patches {
            let start = patch.code_word as usize;
            let site = start
                .checked_add(words)
                .and_then(|end| out.get_mut(start..end))
                .ok_or(Error::OutOfBounds("a colour export patch"))?;
            site.copy_from_slice(&patch.variants[format as usize][..words]);
        }
        Ok(out)
    }
}

/// One attribute a vertex program is handed by its fetch routine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VertexInput {
    /// The usage code. Seen: 6, a generic attribute.
    pub usage: u8,
    /// The register (row) of `ISG1` the attribute is.
    pub row: u8,
    /// Which components the program reads, bit 0 for x.
    pub mask: u8,
    /// The vector register component 0 arrives in. Component k arrives in
    /// the register k above it.
    pub register: u8,
    /// The highest component the program reads.
    pub highest_component: u8,
}

/// One output of a vertex or mesh program: where a parameter export goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageOutput {
    /// The usage code. Seen: 6 a generic attribute, 7 a clip distance, 8
    /// taken to be a cull distance.
    pub usage: u8,
    /// The register (row) of `OSG1` the output is.
    pub row: u8,
    /// The parameter number: `exp param<n>` exports it. In a mesh program
    /// bit 7 marks a per-primitive output.
    pub parameter: u8,
    /// The fourth byte, described as the row again. It differs from the row
    /// in most entries of the title, so its meaning is not known.
    pub row_again: u8,
    /// The parameter slot the next stage reads the output from.
    pub slot: u8,
    /// The flag bits as stored: bit 0 says 16-bit floats, bit 1 that two of
    /// them share each 32-bit word.
    pub flags: u8,
}

/// The stage data of a vertex program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexData {
    /// The attributes a fetch routine loads into vector registers. Empty for
    /// a program that fetches its vertices itself or reads none.
    pub inputs: Vec<VertexInput>,
    /// The parameter exports.
    pub outputs: Vec<StageOutput>,
}

/// The stage data of a compute program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputeData {
    /// `COMPUTE_PGM_RSRC2` as stored; see the methods.
    pub resources: u32,
    /// The thread group's size along x, y and z.
    pub group_size: [u32; 3],
}

impl ComputeData {
    /// How many scalar registers hold user data.
    pub fn user_registers(&self) -> u8 {
        (self.resources >> 1 & 0x1F) as u8
    }

    /// Whether the thread group's index along `axis` (0 for x) is handed to
    /// the program, in the next scalar register after the user data.
    pub fn has_group_id(&self, axis: u8) -> bool {
        self.resources >> (7 + u32::from(axis.min(2))) & 1 != 0
    }

    /// Whether a thread group information register follows the group ids.
    pub fn has_group_info(&self) -> bool {
        self.resources >> 10 & 1 != 0
    }

    /// Whether scratch memory is enabled, which adds one more register.
    pub fn has_scratch(&self) -> bool {
        self.resources & 1 != 0
    }

    /// How many components of the thread's position in its group arrive in
    /// vector registers from `v0` up (1 to 3).
    pub fn thread_id_components(&self) -> u8 {
        (self.resources >> 11 & 3) as u8 + 1
    }

    /// The size of the group-shared memory in bytes.
    pub fn shared_memory_bytes(&self) -> u32 {
        (self.resources >> 15 & 0x1FF) * 512
    }
}

/// The stage data of a mesh program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshData {
    /// The vertex and primitive outputs.
    pub outputs: Vec<StageOutput>,
    /// Eight hardware register values, as stored: `SPI_VS_OUT_CONFIG`,
    /// `SPI_SHADER_POS_FORMAT`, `PA_CL_VS_OUT_CNTL`, `VGT_STRMOUT_CONFIG`,
    /// `SPI_SHADER_PGM_RSRC2_GS`, `VGT_GS_OUT_PRIM_TYPE`,
    /// `VGT_GS_INSTANCE_CNT`, `SPI_SHADER_IDX_FORMAT`. The names are taken
    /// on trust.
    pub registers: [u32; 8],
    /// How many lanes a thread group runs.
    pub group_lanes: u16,
    /// The most vertices a group puts out.
    pub max_vertices: u16,
    /// The most primitives a group puts out.
    pub max_primitives: u16,
    /// How many scalar registers hold user data.
    pub user_registers: u8,
    /// The topology code: 1 lines, 2 triangles.
    pub topology: u8,
}

/// The data particular to one stage's program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageData {
    /// A pixel program's.
    Pixel(PixelData),
    /// A vertex program's.
    Vertex(VertexData),
    /// A compute program's.
    Compute(ComputeData),
    /// A mesh program's.
    Mesh(MeshData),
    /// The stage keeps its data in a layout that is not read: the hull and
    /// geometry stages and the stages merged with them.
    NotRead,
}

impl MachineCode {
    /// Reads the stage data that follows `program`'s record. `program` must
    /// be one of this structure's programs.
    pub fn stage_data(&self, program: &Program) -> Result<StageData, Error> {
        stage_data(&self.payload, program.stage, program.record_offset)
    }
}

/// Reads the stage data of the program whose record starts at
/// `record_offset` of `payload`.
pub fn stage_data(
    payload: &[u8],
    stage: HardwareStage,
    record_offset: u64,
) -> Result<StageData, Error> {
    let start = record_offset
        .checked_add(RECORD_SIZE)
        .ok_or(Error::OutOfBounds("the stage data"))?;
    let data = |size: u64| span(payload, start, size, "the stage data");
    Ok(match stage {
        HardwareStage::Pixel => StageData::Pixel(pixel(data(0xDEC)?)?),
        HardwareStage::Vertex => {
            let data = data(0x18C)?;
            StageData::Vertex(VertexData {
                inputs: vertex_inputs(data)?,
                outputs: outputs(data)?,
            })
        }
        HardwareStage::Compute => {
            let data = data(0x34)?;
            let what = "the compute stage data";
            StageData::Compute(ComputeData {
                resources: u32_at(data, 0x24, what)?,
                group_size: [
                    u32_at(data, 0x28, what)?,
                    u32_at(data, 0x2C, what)?,
                    u32_at(data, 0x30, what)?,
                ],
            })
        }
        HardwareStage::Mesh => {
            let data = data(0x450)?;
            let what = "the mesh stage data";
            let mut registers = [0u32; 8];
            for (index, register) in registers.iter_mut().enumerate() {
                *register = u32_at(data, 0x18C + 4 * index, what)?;
            }
            StageData::Mesh(MeshData {
                outputs: outputs(data)?,
                registers,
                group_lanes: u16_at(data, 0x448, what)?,
                max_vertices: u16_at(data, 0x44A, what)?,
                max_primitives: u16_at(data, 0x44C, what)?,
                user_registers: u8_at(data, 0x44E, what)?,
                topology: u8_at(data, 0x44F, what)?,
            })
        }
        HardwareStage::Local
        | HardwareStage::Hull
        | HardwareStage::Export
        | HardwareStage::Geometry => StageData::NotRead,
    })
}

/// Reads a table's entry count and checks it against the table's room.
fn count(data: &[u8], at: usize, what: &'static str) -> Result<usize, Error> {
    let count = u32_at(data, at, what)? as usize;
    if count > TABLE_ROOM {
        return Err(Error::Malformed(
            "a stage data table states more entries than it has room for",
        ));
    }
    Ok(count)
}

/// Reads a pixel program's stage data.
fn pixel(data: &[u8]) -> Result<PixelData, Error> {
    let what = "the pixel stage data";
    let mut inputs = Vec::new();
    for index in 0..count(data, 0x124, what)? {
        let raw = span(data, (0x24 + 8 * index) as u64, 8, what)?;
        inputs.push(PixelInput {
            usage: raw[0],
            row: raw[1],
            mask: raw[2] & 0xF,
            attribute: raw[3],
            default: raw[4],
            second_row: raw[5],
            slot: raw[6],
            flags: raw[7],
        });
    }
    let patch_words = u32_at(data, 0x148, what)?;
    let mut patches = Vec::new();
    for target in 0..PATCH_TARGETS {
        let at = 0x14C + PATCH_SIZE * target;
        let code_word = u32_at(data, at, what)?;
        if code_word == NO_PATCH {
            continue;
        }
        let mut variants = [[0u32; VARIANT_ROOM]; PATCH_VARIANTS];
        for (number, variant) in variants.iter_mut().enumerate() {
            for (index, word) in variant.iter_mut().enumerate() {
                *word = u32_at(data, at + 4 + 4 * (VARIANT_ROOM * number + index), what)?;
            }
        }
        patches.push(ExportPatch {
            target: target as u8,
            code_word,
            variants,
        });
    }
    Ok(PixelData {
        inputs,
        input_enable: u32_at(data, 0x134, what)?,
        depth_control: u32_at(data, 0x138, what)?,
        patch_words,
        patches,
    })
}

/// Reads a vertex program's input table.
fn vertex_inputs(data: &[u8]) -> Result<Vec<VertexInput>, Error> {
    let what = "the vertex stage data";
    let mut inputs = Vec::new();
    for index in 0..count(data, 0xC4, what)? {
        let raw = span(data, (0x24 + 5 * index) as u64, 5, what)?;
        inputs.push(VertexInput {
            usage: raw[0],
            row: raw[1],
            mask: raw[2] & 0xF,
            register: raw[3],
            highest_component: raw[4],
        });
    }
    Ok(inputs)
}

/// Reads the output table of a vertex or mesh program.
fn outputs(data: &[u8]) -> Result<Vec<StageOutput>, Error> {
    let what = "the stage outputs";
    let mut outputs = Vec::new();
    for index in 0..count(data, 0x188, what)? {
        let raw = span(data, (0xC8 + 6 * index) as u64, 6, what)?;
        outputs.push(StageOutput {
            usage: raw[0],
            row: raw[1],
            parameter: raw[2],
            row_again: raw[3],
            slot: raw[4],
            flags: raw[5],
        });
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::testing::{TestProgram, machine_code};

    /// Builds a structure with one program whose stage data is `data`, and
    /// reads the stage data back.
    fn read(slot: usize, data: Vec<u8>, code: &[u32]) -> (MachineCode, Result<StageData, Error>) {
        let mut program = TestProgram::new(slot, false, code);
        program.stage_data = data;
        let parsed = MachineCode::parse(machine_code(&[program]), false).unwrap();
        let stage = parsed.stage_data(&parsed.programs[0]);
        (parsed, stage)
    }

    fn put32(data: &mut [u8], at: usize, value: u32) {
        data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn a_pixel_programs_inputs_and_patches_are_read() {
        let mut data = vec![0u8; 0xDEC];
        data[0x24..0x2C].copy_from_slice(&[6, 1, 0xFF, 0, 0, 0, 3, 2]);
        data[0x2C..0x34].copy_from_slice(&[6, 2, 0x03, 1, 0, 5, 4, 0x1A]);
        put32(&mut data, 0x124, 2);
        // Weights at the centre, position x and y, front face.
        put32(&mut data, 0x134, 0x1302);
        put32(&mut data, 0x138, 0x51);
        put32(&mut data, 0x148, 2);
        for target in 0..8 {
            put32(&mut data, 0x14C + 404 * target, u32::MAX);
        }
        // Target 1 has a patch at code word 1: variant 9 is two words.
        let patch = 0x14C + 404;
        put32(&mut data, patch, 1);
        put32(&mut data, patch + 4 + 40 * 9, 0xAAAA_0001);
        put32(&mut data, patch + 4 + 40 * 9 + 4, 0xAAAA_0002);
        put32(&mut data, patch + 4 + 40 * 4, 0x4444_0001);
        let code = [0x11, 0x22, 0x33, 0x44];
        let (parsed, stage) = read(0x30, data, &code);
        let StageData::Pixel(pixel) = stage.unwrap() else {
            panic!("not pixel stage data");
        };
        assert_eq!(pixel.inputs.len(), 2);
        assert_eq!(
            pixel.inputs[0],
            PixelInput {
                usage: 6,
                row: 1,
                mask: 0xF,
                attribute: 0,
                default: 0,
                second_row: 0,
                slot: 3,
                flags: 2,
            }
        );
        assert!(!pixel.inputs[0].is_flat() && !pixel.inputs[0].is_half());
        assert!(pixel.inputs[1].is_half() && pixel.inputs[1].is_packed());
        assert_eq!(pixel.inputs[1].second_row, 5);
        assert_eq!(pixel.depth_control, 0x51);
        assert_eq!(
            pixel.system_inputs(),
            [
                (PixelSystemInput::PerspectiveCenter, 0),
                (PixelSystemInput::PositionX, 2),
                (PixelSystemInput::PositionY, 3),
                (PixelSystemInput::FrontFace, 4),
            ]
        );
        assert_eq!(pixel.patches.len(), 1);
        assert_eq!(pixel.patches[0].target, 1);
        assert_eq!(pixel.patches[0].variants[4][0], 0x4444_0001);

        // Patching replaces exactly the patch's words.
        let patched = pixel
            .patched_code(&parsed.programs[0].code, ExportFormat::Abgr32)
            .unwrap();
        assert_eq!(patched, [0x11, 0xAAAA_0001, 0xAAAA_0002, 0x44]);
        let zero = pixel
            .patched_code(&parsed.programs[0].code, ExportFormat::Zero)
            .unwrap();
        assert_eq!(zero, [0x11, 0, 0, 0x44]);
        // A patch that does not fit the code is an error, not a panic.
        assert_eq!(
            pixel.patched_code(&code[..2], ExportFormat::Abgr32),
            Err(Error::OutOfBounds("a colour export patch"))
        );
    }

    #[test]
    fn a_vertex_programs_attributes_and_outputs_are_read() {
        let mut data = vec![0u8; 0x18C];
        data[0x24..0x29].copy_from_slice(&[6, 0, 0x07, 4, 2]);
        data[0x29..0x2E].copy_from_slice(&[6, 2, 0x03, 8, 1]);
        put32(&mut data, 0xC4, 2);
        data[0xC8..0xCE].copy_from_slice(&[6, 1, 0, 9, 0, 0]);
        data[0xCE..0xD4].copy_from_slice(&[7, 3, 1, 3, 1, 3]);
        put32(&mut data, 0x188, 2);
        let (_, stage) = read(0x28, data, &[0xBF81_0000]);
        let StageData::Vertex(vertex) = stage.unwrap() else {
            panic!("not vertex stage data");
        };
        assert_eq!(
            vertex.inputs,
            [
                VertexInput {
                    usage: 6,
                    row: 0,
                    mask: 7,
                    register: 4,
                    highest_component: 2,
                },
                VertexInput {
                    usage: 6,
                    row: 2,
                    mask: 3,
                    register: 8,
                    highest_component: 1,
                },
            ]
        );
        assert_eq!(vertex.outputs.len(), 2);
        assert_eq!((vertex.outputs[0].row, vertex.outputs[0].parameter), (1, 0));
        assert_eq!((vertex.outputs[1].usage, vertex.outputs[1].flags), (7, 3));
    }

    #[test]
    fn a_compute_programs_registers_and_group_size_are_read() {
        let mut data = vec![0u8; 0x34];
        // Scratch, ten user registers, group x and z, group info, three
        // thread components, three granules of shared memory.
        let resources = 1 | 10 << 1 | 0b101 << 7 | 1 << 10 | 2 << 11 | 3 << 15;
        put32(&mut data, 0x24, resources);
        put32(&mut data, 0x28, 8);
        put32(&mut data, 0x2C, 4);
        put32(&mut data, 0x30, 2);
        let (_, stage) = read(0x60, data, &[0xBF81_0000]);
        let StageData::Compute(compute) = stage.unwrap() else {
            panic!("not compute stage data");
        };
        assert_eq!(compute.group_size, [8, 4, 2]);
        assert_eq!(compute.user_registers(), 10);
        assert!(compute.has_group_id(0) && !compute.has_group_id(1) && compute.has_group_id(2));
        assert!(compute.has_group_info() && compute.has_scratch());
        assert_eq!(compute.thread_id_components(), 3);
        assert_eq!(compute.shared_memory_bytes(), 1536);
    }

    #[test]
    fn a_mesh_programs_limits_are_read() {
        let mut data = vec![0u8; 0x450];
        data[0xC8..0xCE].copy_from_slice(&[6, 1, 0x80, 0, 0, 0]);
        put32(&mut data, 0x188, 1);
        put32(&mut data, 0x18C + 8, 0x1234);
        data[0x448..0x450].copy_from_slice(&[128, 0, 64, 0, 126, 0, 17, 2]);
        let (_, stage) = read(0x48, data, &[0xBF81_0000]);
        let StageData::Mesh(mesh) = stage.unwrap() else {
            panic!("not mesh stage data");
        };
        assert_eq!(mesh.outputs[0].parameter, 0x80);
        assert_eq!(mesh.registers[2], 0x1234);
        assert_eq!(
            (mesh.group_lanes, mesh.max_vertices, mesh.max_primitives),
            (128, 64, 126)
        );
        assert_eq!((mesh.user_registers, mesh.topology), (17, 2));
    }

    #[test]
    fn stage_data_that_is_cut_short_or_overfull_is_an_error() {
        // The record is the last thing in the payload: no stage data.
        let (_, stage) = read(0x30, Vec::new(), &[]);
        assert!(stage.is_err());
        // A table that states more entries than it has room for.
        let mut data = vec![0u8; 0x18C];
        put32(&mut data, 0xC4, 33);
        let (_, stage) = read(0x28, data, &[0xBF81_0000]);
        assert!(matches!(stage, Err(Error::Malformed(_))));
        // The stages the title does not use are not read.
        let (_, stage) = read(0x10, Vec::new(), &[0xBF81_0000]);
        assert_eq!(stage, Ok(StageData::NotRead));
    }
}
