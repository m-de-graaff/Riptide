//! Operands: the registers, constants and literal words an instruction names.
//!
//! Most operand fields of the instruction set hold one of a shared set of
//! codes. A scalar field is 7 or 8 bits wide and a vector source field 9
//! bits; the codes mean the same in each:
//!
//! ```text
//! 0-105     scalar registers s0 to s105
//! 106, 107  the two halves of VCC, the vector condition code
//! 108-123   trap handler registers ttmp0 to ttmp15
//! 124       M0
//! 125       null: reads as zero, writes are dropped
//! 126, 127  the two halves of EXEC, the mask of active lanes
//! 128       the constant 0
//! 129-192   the constants 1 to 64
//! 193-208   the constants -1 to -16
//! 233, 234  in source 0 of a vector operation: an eight-lane permutation
//!           word follows (234 also fetches inactive lanes)
//! 235-239   memory aperture bases and limits, and the exiting wave id
//! 240-248   the float constants 0.5, -0.5, 1.0, -1.0, 2.0, -2.0, 4.0, -4.0
//!           and 1/(2*pi)
//! 249, 250  in source 0 of a vector operation: a sub-register word or a
//!           lane permutation word follows
//! 251-253   VCC is zero, EXEC is zero, the scalar condition code
//! 254       direct access to group-shared memory
//! 255       a 32-bit literal follows the instruction
//! 256-511   vector registers v0 to v255
//! ```

use super::WaveSize;

/// The code that means a literal word follows the instruction.
pub(super) const LITERAL: u16 = 255;
/// The code of the null register, which is also how a memory instruction
/// says it has no scalar offset register.
pub(super) const NULL: u16 = 125;

/// Which part of a 64-bit special register an operand names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Half {
    /// The low 32 bits.
    Low,
    /// The high 32 bits.
    High,
    /// All 64 bits.
    Both,
}

/// A float constant that an operand code stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloatConstant {
    /// 0.5
    Half,
    /// -0.5
    MinusHalf,
    /// 1.0
    One,
    /// -1.0
    MinusOne,
    /// 2.0
    Two,
    /// -2.0
    MinusTwo,
    /// 4.0
    Four,
    /// -4.0
    MinusFour,
    /// 1/(2*pi)
    InverseTwoPi,
}

impl FloatConstant {
    /// The constant as a 32-bit float.
    pub fn value(self) -> f32 {
        match self {
            FloatConstant::Half => 0.5,
            FloatConstant::MinusHalf => -0.5,
            FloatConstant::One => 1.0,
            FloatConstant::MinusOne => -1.0,
            FloatConstant::Two => 2.0,
            FloatConstant::MinusTwo => -2.0,
            FloatConstant::Four => 4.0,
            FloatConstant::MinusFour => -4.0,
            FloatConstant::InverseTwoPi => 0.159_154_94,
        }
    }

    /// The constant as assemblers write it.
    fn text(self) -> &'static str {
        match self {
            FloatConstant::Half => "0.5",
            FloatConstant::MinusHalf => "-0.5",
            FloatConstant::One => "1.0",
            FloatConstant::MinusOne => "-1.0",
            FloatConstant::Two => "2.0",
            FloatConstant::MinusTwo => "-2.0",
            FloatConstant::Four => "4.0",
            FloatConstant::MinusFour => "-4.0",
            FloatConstant::InverseTwoPi => "0.15915494",
        }
    }
}

/// One operand of an instruction.
///
/// A register operand carries how many consecutive 32-bit registers it
/// spans, because the same register field means `s4` to one instruction and
/// `s[4:5]` to another. That width comes from the instruction, not from the
/// field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operand {
    /// Scalar registers `first` to `first + count - 1`.
    Scalar {
        /// The first register, 0 to 105.
        first: u8,
        /// How many registers.
        count: u8,
    },
    /// Vector registers `first` to `first + count - 1`.
    Vector {
        /// The first register, 0 to 255.
        first: u8,
        /// How many registers.
        count: u8,
    },
    /// Trap handler registers `first` to `first + count - 1`.
    Trap {
        /// The first register, 0 to 15.
        first: u8,
        /// How many registers.
        count: u8,
    },
    /// The vector condition code, a lane mask.
    Vcc(Half),
    /// The mask of active lanes.
    Exec(Half),
    /// The register M0, which some instructions read implicitly.
    M0,
    /// No register: reads as zero and drops what is written.
    Null,
    /// 1 if VCC is zero, as a source.
    VccZero,
    /// 1 if EXEC is zero, as a source.
    ExecZero,
    /// The scalar condition code, as a source.
    Scc,
    /// Direct access to group-shared memory, as a source.
    LdsDirect,
    /// An integer constant from -16 to 64.
    Integer(i8),
    /// A float constant.
    Float(FloatConstant),
    /// The 32-bit literal word that follows the instruction.
    Literal(u32),
    /// A value held in a field of the instruction itself, where the field is
    /// not an operand code: the probe mode of an address translation probe,
    /// or the parameter of a flat interpolation.
    Immediate(u32),
    /// A code that names nothing a shader computes with: the memory aperture
    /// bases and limits (235-238), the exiting wave id (239), and the codes
    /// this generation leaves unused.
    Other(u16),
}

impl Operand {
    /// Reads a scalar register code (0 to 127) as an operand `count`
    /// registers wide.
    pub(super) fn scalar(code: u8, count: u8) -> Operand {
        let wide = count >= 2;
        match code {
            0..=105 => Operand::Scalar { first: code, count },
            106 => Operand::Vcc(if wide { Half::Both } else { Half::Low }),
            107 => Operand::Vcc(Half::High),
            108..=123 => Operand::Trap {
                first: code - 108,
                count,
            },
            124 => Operand::M0,
            125 => Operand::Null,
            126 => Operand::Exec(if wide { Half::Both } else { Half::Low }),
            127 => Operand::Exec(Half::High),
            _ => Operand::Other(u16::from(code)),
        }
    }

    /// Reads a source code of up to 9 bits. `literal` is the word after the
    /// instruction, used when the code asks for one.
    pub(super) fn source(code: u16, count: u8, literal: u32) -> Operand {
        match code {
            0..=127 => Operand::scalar(code as u8, count),
            128..=192 => Operand::Integer((code - 128) as i8),
            193..=208 => Operand::Integer(-((code - 192) as i8)),
            240 => Operand::Float(FloatConstant::Half),
            241 => Operand::Float(FloatConstant::MinusHalf),
            242 => Operand::Float(FloatConstant::One),
            243 => Operand::Float(FloatConstant::MinusOne),
            244 => Operand::Float(FloatConstant::Two),
            245 => Operand::Float(FloatConstant::MinusTwo),
            246 => Operand::Float(FloatConstant::Four),
            247 => Operand::Float(FloatConstant::MinusFour),
            248 => Operand::Float(FloatConstant::InverseTwoPi),
            251 => Operand::VccZero,
            252 => Operand::ExecZero,
            253 => Operand::Scc,
            254 => Operand::LdsDirect,
            LITERAL => Operand::Literal(literal),
            256..=511 => Operand::vector((code - 256) as u8, count),
            _ => Operand::Other(code),
        }
    }

    /// A vector register operand `count` registers wide.
    pub(super) fn vector(first: u8, count: u8) -> Operand {
        Operand::Vector { first, count }
    }

    /// The VCC register as a lane mask of the given wave size.
    pub(super) fn vcc(wave: WaveSize) -> Operand {
        Operand::scalar(106, wave.mask_words())
    }

    /// Whether the operand is a register (of any kind) and not a constant.
    pub fn is_register(&self) -> bool {
        matches!(
            self,
            Operand::Scalar { .. }
                | Operand::Vector { .. }
                | Operand::Trap { .. }
                | Operand::Vcc(_)
                | Operand::Exec(_)
                | Operand::M0
                | Operand::Null
        )
    }
}

/// Writes a register range the way assemblers do: `s4`, or `s[4:5]`.
fn range(f: &mut std::fmt::Formatter<'_>, prefix: &str, first: u8, count: u8) -> std::fmt::Result {
    if count <= 1 {
        write!(f, "{prefix}{first}")
    } else {
        let last = u16::from(first) + u16::from(count) - 1;
        write!(f, "{prefix}[{first}:{last}]")
    }
}

impl std::fmt::Display for Operand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Operand::Scalar { first, count } => range(f, "s", first, count),
            Operand::Vector { first, count } => range(f, "v", first, count),
            Operand::Trap { first, count } => range(f, "ttmp", first, count),
            Operand::Vcc(Half::Low) => f.write_str("vcc_lo"),
            Operand::Vcc(Half::High) => f.write_str("vcc_hi"),
            Operand::Vcc(Half::Both) => f.write_str("vcc"),
            Operand::Exec(Half::Low) => f.write_str("exec_lo"),
            Operand::Exec(Half::High) => f.write_str("exec_hi"),
            Operand::Exec(Half::Both) => f.write_str("exec"),
            Operand::M0 => f.write_str("m0"),
            Operand::Null => f.write_str("null"),
            Operand::VccZero => f.write_str("src_vccz"),
            Operand::ExecZero => f.write_str("src_execz"),
            Operand::Scc => f.write_str("src_scc"),
            Operand::LdsDirect => f.write_str("src_lds_direct"),
            Operand::Integer(value) => write!(f, "{value}"),
            Operand::Float(value) => f.write_str(value.text()),
            Operand::Literal(word) => write!(f, "{word:#x}"),
            Operand::Immediate(value) => write!(f, "{value:#x}"),
            Operand::Other(235) => f.write_str("src_shared_base"),
            Operand::Other(236) => f.write_str("src_shared_limit"),
            Operand::Other(237) => f.write_str("src_private_base"),
            Operand::Other(238) => f.write_str("src_private_limit"),
            Operand::Other(239) => f.write_str("src_pops_exiting_wave_id"),
            Operand::Other(code) => write!(f, "reserved_{code}"),
        }
    }
}
