//! The machine code of the console GPU: decoding and printing instructions.
//!
//! The console's GPU belongs to the generation AMD's open-source drivers call
//! GFX10 (RDNA 1 and RDNA 2). Its instruction set is public knowledge: LLVM's
//! AMDGPU backend and Mesa's ACO compiler both describe it. A program is a
//! sequence of 32-bit words. An instruction is one word, two words, or more
//! when a literal constant or extra operand words follow it. The top bits of
//! the first word say which [`Format`] the instruction has:
//!
//! ```text
//! 0.......   two-source vector operation      (VOP2), except
//! 0111111.   one-source vector operation      (VOP1)
//! 0111110.   vector comparison                (VOPC)
//! 10......   two-source scalar operation      (SOP2), except
//! 1011....   scalar operation with immediate  (SOPK)
//! 101111101  one-source scalar operation      (SOP1)
//! 101111110  scalar comparison                (SOPC)
//! 101111111  program control                  (SOPP)
//! 110010     attribute interpolation          (VINTRP)
//! 110011     packed vector operation          (VOP3P)
//! 110101     three-source vector operation    (VOP3A, VOP3B)
//! 110110     group-shared memory              (DS)
//! 110111     memory by address                (FLAT, GLOBAL, SCRATCH)
//! 111000     untyped buffer                   (MUBUF)
//! 111010     typed buffer                     (MTBUF)
//! 111100     image                            (MIMG)
//! 111101     scalar memory                    (SMEM)
//! 111110     export                           (EXP)
//! ```
//!
//! [`decode_at`] decodes the instruction at one word and never fails: a
//! known format with an opcode this module has no name for, a reserved
//! format, and an instruction cut off by the end of the code all come back
//! as an [`Instruction`] that keeps its words and has no mnemonic, so that a
//! program can still be walked.
//!
//! Two functions decode whole programs. [`decode_linear`] decodes one
//! instruction after another from the first word to the last. [`walk`]
//! follows the program's control flow from the first word instead, because
//! compilers place data (tables of constants) after the end of a program,
//! where a linear decode would read it as instructions.
//!
//! Instructions print in the syntax of LLVM's assembler through their
//! `Display` implementation, and [`listing`] prints a whole program with
//! offsets. The text is meant for reading and for comparison with other
//! tools. It was written from knowledge of that syntax and has not been
//! compared mechanically with LLVM's output; the unusual operand forms
//! (sub-register and lane-permutation modifiers, typed buffer formats,
//! packed-operation selects) are the likeliest to differ in detail.
//!
//! What is not built: no instruction is given a meaning here. Decoding says
//! what an instruction is and what it names, not what it computes.

mod decode;
mod opcodes;
mod operand;
mod shape;
mod text;
mod walk;

#[cfg(test)]
mod tests;

pub use decode::decode_at;
#[cfg(test)]
pub(crate) use opcodes::number_of;
pub use operand::{FloatConstant, Half, Operand};
pub use text::listing;
pub(crate) use walk::far_jump as far_jump_target;
pub use walk::{Walk, decode_linear, walk};

/// How many lanes (shader invocations) one wave of a program runs.
///
/// The size decides how wide a lane mask is: one 32-bit register or a pair.
/// Instructions that produce or consume lane masks therefore read their
/// operands differently in the two sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaveSize {
    /// 32 lanes; a lane mask is one register.
    Wave32,
    /// 64 lanes; a lane mask is a pair of registers.
    Wave64,
}

impl WaveSize {
    /// How many 32-bit registers a lane mask occupies.
    pub fn mask_words(self) -> u8 {
        match self {
            WaveSize::Wave32 => 1,
            WaveSize::Wave64 => 2,
        }
    }
}

/// The encoding format of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Format {
    /// Scalar operation with two sources.
    Sop2,
    /// Scalar operation with a 16-bit immediate.
    Sopk,
    /// Scalar operation with one source.
    Sop1,
    /// Scalar comparison.
    Sopc,
    /// Program control.
    Sopp,
    /// Scalar memory access.
    Smem,
    /// Vector operation with two sources.
    Vop2,
    /// Vector operation with one source.
    Vop1,
    /// Vector comparison.
    Vopc,
    /// Vector operation with three sources and per-source modifiers.
    Vop3a,
    /// The three-source encoding with a second, scalar result (a carry or a
    /// flag mask) in place of some modifiers.
    Vop3b,
    /// Packed vector operation: two 16-bit halves per register.
    Vop3p,
    /// Attribute interpolation of pixel shaders.
    Vintrp,
    /// Group-shared memory access.
    Ds,
    /// Memory access by 64-bit address in any address space.
    Flat,
    /// Memory access by 64-bit address in global memory.
    Global,
    /// Memory access in the wave's scratch memory.
    Scratch,
    /// Untyped buffer access.
    Mubuf,
    /// Typed buffer access.
    Mtbuf,
    /// Image access and sampling.
    Mimg,
    /// Export of a shader stage's results.
    Exp,
    /// A first word whose top bits select no format of this generation.
    Unknown,
}

impl Format {
    /// The conventional name of the format.
    pub fn name(self) -> &'static str {
        match self {
            Format::Sop2 => "SOP2",
            Format::Sopk => "SOPK",
            Format::Sop1 => "SOP1",
            Format::Sopc => "SOPC",
            Format::Sopp => "SOPP",
            Format::Smem => "SMEM",
            Format::Vop2 => "VOP2",
            Format::Vop1 => "VOP1",
            Format::Vopc => "VOPC",
            Format::Vop3a => "VOP3A",
            Format::Vop3b => "VOP3B",
            Format::Vop3p => "VOP3P",
            Format::Vintrp => "VINTRP",
            Format::Ds => "DS",
            Format::Flat => "FLAT",
            Format::Global => "GLOBAL",
            Format::Scratch => "SCRATCH",
            Format::Mubuf => "MUBUF",
            Format::Mtbuf => "MTBUF",
            Format::Mimg => "MIMG",
            Format::Exp => "EXP",
            Format::Unknown => "UNKNOWN",
        }
    }
}

/// What part an operand plays in its instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The register the instruction writes.
    Result,
    /// A second register the instruction writes: the carry out of an
    /// addition, or the flag mask of the three-source operations that have
    /// one.
    SecondResult,
    /// A value the instruction computes with.
    Source,
    /// The value a memory instruction stores, or the operand of an atomic
    /// operation. When an atomic operation returns the old value it returns
    /// it in the same register.
    Data,
    /// The address, index or coordinates of a memory or image access.
    Address,
    /// The scalar register pair or quad that holds a base address or a
    /// buffer descriptor for scalar and address-based memory access.
    Base,
    /// A scalar register added to the address as an offset.
    Offset,
    /// The scalar registers that hold a buffer or image descriptor.
    Resource,
    /// The scalar registers that hold a sampler descriptor.
    Sampler,
}

/// One operand of an instruction and the part it plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arg {
    /// The part the operand plays.
    pub role: Role,
    /// The operand.
    pub operand: Operand,
}

/// The sub-register word of a vector operation (the encoding AMD calls
/// SDWA): it selects a byte or word of each source and of the result.
///
/// Selects are 0 to 3 for a byte, 4 and 5 for a 16-bit word, 6 for the whole
/// register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubRegister {
    /// Which part of the result is written. Comparisons have none.
    pub result_select: u8,
    /// What happens to the rest of the result: 0 zero, 1 sign-extend,
    /// 2 keep.
    pub result_unused: u8,
    /// Which part of each source is read.
    pub source_select: [u8; 2],
    /// Whether each source's part is sign-extended.
    pub source_sign_extend: [bool; 2],
}

/// The lane permutation of a vector operation (the encodings AMD calls DPP):
/// source 0 is read from other lanes of the wave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanePermutation {
    /// A permutation within rows of 16 lanes, chosen by a control number.
    Rows {
        /// The control number: 0x00 to 0xFF permute within each group of 4
        /// lanes (2 bits per lane), 0x101 to 0x10F shift a row left, 0x111
        /// to 0x11F shift it right, 0x121 to 0x12F rotate it right, 0x140
        /// mirrors a row and 0x141 mirrors each half of it.
        control: u16,
        /// Whether lanes that are not active are read as sources.
        fetch_inactive: bool,
        /// Whether a lane with no source lane gets zero instead of keeping
        /// its value.
        bound_control: bool,
        /// Which groups of 4 lanes within a row are written.
        bank_mask: u8,
        /// Which rows are written.
        row_mask: u8,
    },
    /// A free permutation within each group of 8 lanes.
    Eight {
        /// The source lane of each of the 8 lanes.
        lanes: [u8; 8],
        /// Whether lanes that are not active are read as sources.
        fetch_inactive: bool,
    },
}

/// The modifiers of a vector operation.
///
/// Bit i of a per-source mask applies to the i-th operand with the role
/// [`Role::Source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorModifiers {
    /// Sources whose absolute value is taken.
    pub absolute: u8,
    /// Sources that are negated. For packed operations, the low halves.
    pub negate: u8,
    /// Packed operations: sources whose high half is negated.
    pub negate_high: u8,
    /// Sources (bits 0 to 2) and result (bit 3) that use their high 16 bits.
    /// For packed operations, which half of each source feeds the low half
    /// of the result.
    pub select: u8,
    /// Packed operations: which half of each source feeds the high half of
    /// the result.
    pub select_high: u8,
    /// Whether the result is clamped to the range of its type.
    pub clamp: bool,
    /// The output scale: 0 none, 1 times 2, 2 times 4, 3 divided by 2.
    pub output_scale: u8,
    /// The sub-register word, if the instruction has one.
    pub sub_register: Option<SubRegister>,
    /// The lane permutation, if the instruction has one.
    pub permutation: Option<LanePermutation>,
}

/// The fields of an instruction that are not operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// The instruction has no such fields, or was not recognised.
    None,
    /// The 16-bit immediate of a scalar-immediate or program-control
    /// instruction: a constant, a branch distance in words, wait counts, a
    /// message.
    Immediate(u16),
    /// The modifiers of a vector operation.
    Vector(VectorModifiers),
    /// Scalar memory access.
    ScalarMemory {
        /// The constant byte offset, a signed 21-bit number.
        offset: i32,
        /// The cache policy bit AMD calls GLC (globally coherent).
        glc: bool,
        /// The cache policy bit AMD calls DLC (device coherent).
        dlc: bool,
    },
    /// Group-shared memory access.
    Lds {
        /// The first offset byte. For instructions on one address the two
        /// bytes form one 16-bit offset.
        offset0: u8,
        /// The second offset byte.
        offset1: u8,
        /// Whether the access goes to global data share instead.
        gds: bool,
    },
    /// Buffer access, typed or untyped.
    Buffer {
        /// The constant byte offset, 12 bits.
        offset: u16,
        /// Whether the address register holds a byte offset.
        offset_enable: bool,
        /// Whether the address register holds an element index.
        index_enable: bool,
        /// The cache policy bit GLC.
        glc: bool,
        /// The cache policy bit DLC.
        dlc: bool,
        /// The cache policy bit SLC (system level coherent).
        slc: bool,
        /// Whether a texture-fail word is returned after the data.
        tfe: bool,
        /// The data format number of a typed buffer access.
        format: Option<u8>,
    },
    /// Image access.
    Image {
        /// Which of the four components are read or written.
        component_mask: u8,
        /// The image dimension: 0 1D, 1 2D, 2 3D, 3 cube, 4 1D array,
        /// 5 2D array, 6 2D multisampled, 7 2D multisampled array.
        dimension: u8,
        /// Whether coordinates are in texels instead of normalised.
        unnormalized: bool,
        /// The cache policy bit GLC.
        glc: bool,
        /// The cache policy bit SLC.
        slc: bool,
        /// The cache policy bit DLC.
        dlc: bool,
        /// Whether the descriptor is 128 bits instead of 256.
        r128: bool,
        /// Whether coordinates are 16-bit.
        a16: bool,
        /// Whether data is 16-bit.
        d16: bool,
        /// Whether a texture-fail word is returned after the data.
        tfe: bool,
        /// Whether a LOD-warning word is returned after the data.
        lwe: bool,
        /// How many extra words hold address registers that are not
        /// consecutive (0 to 3).
        extra_address_words: u8,
    },
    /// Memory access by address.
    Flat {
        /// The constant byte offset: unsigned 11 bits for [`Format::Flat`],
        /// signed 12 bits for the other two.
        offset: i16,
        /// Whether the access goes to group-shared memory.
        lds: bool,
        /// The cache policy bit GLC. On an atomic operation it means the
        /// old value is returned.
        glc: bool,
        /// The cache policy bit SLC.
        slc: bool,
        /// The cache policy bit DLC.
        dlc: bool,
    },
    /// Attribute interpolation.
    Interpolation {
        /// The attribute number.
        attribute: u8,
        /// The component of the attribute, 0 for x to 3 for w.
        channel: u8,
    },
    /// Export.
    Export {
        /// Where the values go: 0 to 7 colour targets, 8 depth, 9 nowhere,
        /// 12 to 16 positions, 20 primitive data, 32 to 63 parameters.
        target: u8,
        /// Which of the four values are exported.
        enabled: u8,
        /// Whether the values are pairs of 16-bit halves packed into two
        /// registers.
        compressed: bool,
        /// Whether this is the last export of its kind in the program.
        done: bool,
        /// Whether EXEC is used as the valid mask of the pixels.
        valid_mask: bool,
    },
}

/// One decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// The byte offset of the instruction's first word in its program.
    pub offset: u32,
    /// The encoding format.
    pub format: Format,
    /// The opcode number as the format's opcode field holds it.
    pub opcode: u16,
    /// The mnemonic, or `None` when the opcode has no name here, the format
    /// is not one of this generation, or the instruction is cut off.
    pub mnemonic: Option<&'static str>,
    /// Every word of the instruction, including literals and extra words.
    pub words: Vec<u32>,
    /// Whether the code ends before the instruction does. Its words are then
    /// the ones that remain.
    pub truncated: bool,
    /// The operands in the order assemblers write them.
    pub args: Vec<Arg>,
    /// The fields that are not operands.
    pub detail: Detail,
}

impl Instruction {
    /// Whether the instruction could not be named.
    pub fn is_unknown(&self) -> bool {
        self.mnemonic.is_none()
    }

    /// The byte offset just after the instruction.
    pub fn end(&self) -> u32 {
        self.offset.wrapping_add(4 * self.words.len() as u32)
    }

    /// The first operand with the given role.
    pub fn arg(&self, role: Role) -> Option<Operand> {
        self.args
            .iter()
            .find(|arg| arg.role == role)
            .map(|arg| arg.operand)
    }

    /// The operands with the role [`Role::Source`], in order.
    pub fn sources(&self) -> impl Iterator<Item = Operand> + '_ {
        self.args
            .iter()
            .filter(|arg| arg.role == Role::Source)
            .map(|arg| arg.operand)
    }

    /// Whether the encoding is the three-source one of an operation that
    /// also has a shorter encoding. Assemblers mark such an instruction with
    /// the suffix `_e64`.
    pub fn is_long_form(&self) -> bool {
        matches!(self.format, Format::Vop3a | Format::Vop3b)
            && (self.opcode < opcodes::VOP3_OWN_BASE
                || (opcodes::VOP3_VOP1_BASE..opcodes::VOP3_VOP1_END).contains(&self.opcode))
    }

    /// For a branch or call with a constant distance, the byte offset it
    /// goes to. The distance counts words from the instruction after the
    /// branch. The result can lie outside the program.
    pub fn branch_target(&self) -> Option<i64> {
        let name = self.mnemonic?;
        let relative = match self.format {
            Format::Sopp => name == "s_branch" || name.starts_with("s_cbranch_"),
            Format::Sopk => name == "s_call_b64",
            _ => false,
        };
        let Detail::Immediate(distance) = self.detail else {
            return None;
        };
        relative.then(|| i64::from(self.end()) + 4 * i64::from(distance as i16))
    }

    /// Whether execution cannot continue to the next instruction: the
    /// instruction ends the program, or jumps without condition.
    pub fn ends_block(&self) -> bool {
        matches!(
            (self.format, self.mnemonic),
            (
                Format::Sopp,
                Some(
                    "s_endpgm"
                        | "s_endpgm_saved"
                        | "s_endpgm_ordered_ps_done"
                        | "s_code_end"
                        | "s_branch"
                )
            ) | (Format::Sop1, Some("s_setpc_b64" | "s_rfe_b64"))
        )
    }
}
