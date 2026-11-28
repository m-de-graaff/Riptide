//! What each operation of the intermediate form computes, as plain
//! functions on words.
//!
//! These functions are the definition of the operations: the reference
//! interpreter calls them, and an emitter has to produce host code with the
//! same results. The console's rules that differ from a host's are from
//! section 11 of the research note and from AMD's public description of the
//! instruction set. None of them has been measured on hardware by this
//! project; where the hardware is known to be approximate (reciprocal,
//! square roots, exponentials, sine and cosine) the functions here give the
//! correctly rounded result.

use super::{BinaryOp, CompareOp, TernaryOp, UnaryOp};

/// Reads a word as a float.
fn f(word: u32) -> f32 {
    f32::from_bits(word)
}

/// Converts a 32-bit float to the bits of a 16-bit float. `toward_zero`
/// chooses between rounding toward zero and rounding to nearest even.
pub fn half_from_float(value: f32, toward_zero: bool) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xFF) as i32;
    let fraction = bits & 0x7F_FFFF;
    if exponent == 0xFF {
        // Infinity, or a NaN, which keeps its top fraction bits and stays a
        // NaN.
        let payload = if fraction == 0 {
            0
        } else {
            0x200 | (fraction >> 13) as u16
        };
        return sign | 0x7C00 | payload;
    }
    // The value is 1.fraction * 2^(exponent - 127), or a denormal.
    let unbiased = exponent - 127;
    if unbiased > 15 {
        // Too large: infinity, or the largest finite value when rounding
        // toward zero.
        return sign | if toward_zero { 0x7BFF } else { 0x7C00 };
    }
    // The significand with its leading one, 24 bits; zero and denormal
    // floats are far below the smallest half and round to zero.
    if exponent == 0 {
        return sign;
    }
    let significand = fraction | 0x80_0000;
    // How many low bits are dropped: 13 for a normal half, more for a
    // denormal one.
    let shift = if unbiased >= -14 {
        13
    } else {
        13 + (-14 - unbiased)
    };
    if shift > 24 {
        return sign;
    }
    let shift = shift as u32;
    let kept = significand >> shift;
    let dropped = significand & ((1u32 << shift) - 1);
    let halfway = 1u32 << (shift - 1);
    let mut result = if unbiased >= -14 {
        // Normal: the exponent field and the 10 fraction bits. The leading
        // one of `kept` adds into the exponent field, which is why the
        // field starts one lower.
        (((unbiased + 14) as u32) << 10) + kept
    } else {
        kept
    };
    if !toward_zero && (dropped > halfway || (dropped == halfway && result & 1 != 0)) {
        // A carry out of the fraction moves into the exponent, which is the
        // right next value, infinity included.
        result += 1;
    }
    sign | result as u16
}

/// Converts the bits of a 16-bit float to a 32-bit float.
pub fn float_from_half(half: u16) -> f32 {
    let sign = u32::from(half & 0x8000) << 16;
    let exponent = u32::from(half >> 10 & 0x1F);
    let fraction = u32::from(half & 0x3FF);
    let bits = match (exponent, fraction) {
        (0, 0) => sign,
        (0, _) => {
            // A denormal: shift the fraction up until its leading one is in
            // the hidden place.
            let shift = fraction.leading_zeros() - 21;
            let fraction = (fraction << shift) & 0x3FF;
            sign | (113 - shift) << 23 | fraction << 13
        }
        (0x1F, _) => sign | 0x7F80_0000 | fraction << 13,
        _ => sign | (exponent + 112) << 23 | fraction << 13,
    };
    f32::from_bits(bits)
}

/// The smaller float by the console's rule: a NaN loses, and -0 is below +0.
fn float_min(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        b
    } else if b.is_nan() {
        a
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() { a } else { b }
    } else if a < b {
        a
    } else {
        b
    }
}

/// The larger float by the console's rule: a NaN loses, and +0 is above -0.
fn float_max(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        b
    } else if b.is_nan() {
        a
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_positive() { a } else { b }
    } else if a > b {
        a
    } else {
        b
    }
}

/// Sine or cosine of an angle in turns.
fn turns(value: f32, cosine: bool) -> f32 {
    if value.is_nan() {
        return value;
    }
    if !(-256.0..=256.0).contains(&value) {
        return if cosine { 1.0 } else { 0.0 };
    }
    // The fraction of a turn is exact in a float, so the quarter points can
    // be told exactly.
    let turn = f64::from(value);
    let fraction = turn - turn.floor();
    let quarter = fraction * 4.0;
    if quarter == quarter.floor() {
        let table = if cosine {
            [1.0, 0.0, -1.0, 0.0]
        } else {
            [0.0, 1.0, 0.0, -1.0]
        };
        return table[quarter as usize & 3];
    }
    let angle = fraction * std::f64::consts::TAU;
    (if cosine { angle.cos() } else { angle.sin() }) as f32
}

/// Evaluates an operation on one word.
pub fn unary(op: UnaryOp, a: u32) -> u32 {
    match op {
        UnaryOp::Not => !a,
        UnaryOp::CountBits => a.count_ones(),
        UnaryOp::FindLowBit => {
            if a == 0 {
                u32::MAX
            } else {
                a.trailing_zeros()
            }
        }
        UnaryOp::LeadingZeros => {
            if a == 0 {
                u32::MAX
            } else {
                a.leading_zeros()
            }
        }
        UnaryOp::LeadingSignBits => {
            if a == 0 || a == u32::MAX {
                u32::MAX
            } else if a >> 31 != 0 {
                (!a).leading_zeros()
            } else {
                a.leading_zeros()
            }
        }
        UnaryOp::ReverseBits => a.reverse_bits(),
        UnaryOp::Saturate => {
            let value = f(a);
            if value.is_nan() {
                0
            } else {
                value.clamp(0.0, 1.0).to_bits()
            }
        }
        UnaryOp::Reciprocal => (1.0 / f(a)).to_bits(),
        UnaryOp::ReciprocalSqrt => ((1.0 / f64::from(f(a)).sqrt()) as f32).to_bits(),
        UnaryOp::Sqrt => f(a).sqrt().to_bits(),
        UnaryOp::Exp2 => (f64::from(f(a)).exp2() as f32).to_bits(),
        UnaryOp::Log2 => (f64::from(f(a)).log2() as f32).to_bits(),
        UnaryOp::SinTurns => turns(f(a), false).to_bits(),
        UnaryOp::CosTurns => turns(f(a), true).to_bits(),
        UnaryOp::Floor => f(a).floor().to_bits(),
        UnaryOp::Ceil => f(a).ceil().to_bits(),
        UnaryOp::Trunc => f(a).trunc().to_bits(),
        UnaryOp::RoundEven => f(a).round_ties_even().to_bits(),
        UnaryOp::Fract => {
            let value = f(a);
            let fraction = value - value.floor();
            if fraction >= 1.0 {
                // The largest float below one.
                0x3F7F_FFFF
            } else {
                fraction.to_bits()
            }
        }
        UnaryOp::FloatFromSigned => (a as i32 as f32).to_bits(),
        UnaryOp::FloatFromUnsigned => (a as f32).to_bits(),
        // Rust's float-to-integer casts saturate and give 0 for a NaN, which
        // is the console's rule.
        UnaryOp::SignedFromFloat => f(a) as i32 as u32,
        UnaryOp::UnsignedFromFloat => f(a) as u32,
        UnaryOp::HalfFromFloat => u32::from(half_from_float(f(a), false)),
        UnaryOp::HalfFromFloatTowardZero => u32::from(half_from_float(f(a), true)),
        UnaryOp::FloatFromHalf => float_from_half(a as u16).to_bits(),
    }
}

/// Evaluates an operation on two words.
pub fn binary(op: BinaryOp, a: u32, b: u32) -> u32 {
    match op {
        BinaryOp::Add => a.wrapping_add(b),
        BinaryOp::Sub => a.wrapping_sub(b),
        BinaryOp::Mul => a.wrapping_mul(b),
        BinaryOp::MulHighUnsigned => ((u64::from(a) * u64::from(b)) >> 32) as u32,
        BinaryOp::MulHighSigned => ((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32,
        BinaryOp::And => a & b,
        BinaryOp::Or => a | b,
        BinaryOp::Xor => a ^ b,
        BinaryOp::ShiftLeft => a << (b & 31),
        BinaryOp::ShiftRight => a >> (b & 31),
        BinaryOp::ShiftRightSigned => ((a as i32) >> (b & 31)) as u32,
        BinaryOp::MinUnsigned => a.min(b),
        BinaryOp::MaxUnsigned => a.max(b),
        BinaryOp::MinSigned => (a as i32).min(b as i32) as u32,
        BinaryOp::MaxSigned => (a as i32).max(b as i32) as u32,
        BinaryOp::FloatAdd => (f(a) + f(b)).to_bits(),
        BinaryOp::FloatSub => (f(a) - f(b)).to_bits(),
        BinaryOp::FloatMul => (f(a) * f(b)).to_bits(),
        BinaryOp::FloatMulLegacy => {
            if f(a) == 0.0 || f(b) == 0.0 {
                0
            } else {
                (f(a) * f(b)).to_bits()
            }
        }
        BinaryOp::FloatMin => float_min(f(a), f(b)).to_bits(),
        BinaryOp::FloatMax => float_max(f(a), f(b)).to_bits(),
        BinaryOp::FloatScale => {
            let exponent = (b as i32).clamp(-400, 400);
            ((f64::from(f(a)) * 2f64.powi(exponent)) as f32).to_bits()
        }
    }
}

/// The three components of a cube direction and which axis is the major one
/// (0 for x, 1 for y, 2 for z).
fn cube_axis(x: f32, y: f32, z: f32) -> u8 {
    if z.abs() >= x.abs() && z.abs() >= y.abs() {
        2
    } else if y.abs() >= x.abs() {
        1
    } else {
        0
    }
}

/// Picks one byte for [`TernaryOp::PermuteBytes`].
fn permute_byte(bytes: [u8; 8], select: u8) -> u8 {
    let sign = |index: usize| if bytes[index] & 0x80 != 0 { 0xFF } else { 0 };
    match select {
        0..=7 => bytes[usize::from(select)],
        8 => sign(1),
        9 => sign(3),
        10 => sign(5),
        11 => sign(7),
        12 => 0,
        _ => 0xFF,
    }
}

/// Evaluates an operation on three words.
pub fn ternary(op: TernaryOp, a: u32, b: u32, c: u32) -> u32 {
    match op {
        TernaryOp::FloatFused => f(a).mul_add(f(b), f(c)).to_bits(),
        TernaryOp::FloatMulAdd => (f(a) * f(b) + f(c)).to_bits(),
        TernaryOp::FloatMedian => {
            let (x, y, z) = (f(a), f(b), f(c));
            if x.is_nan() || y.is_nan() || z.is_nan() {
                float_min(float_min(x, y), z).to_bits()
            } else {
                let largest = float_max(float_max(x, y), z);
                if largest.to_bits() == x.to_bits() {
                    float_max(y, z).to_bits()
                } else if largest.to_bits() == y.to_bits() {
                    float_max(x, z).to_bits()
                } else {
                    float_max(x, y).to_bits()
                }
            }
        }
        TernaryOp::PermuteBytes => {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&b.to_le_bytes());
            bytes[4..].copy_from_slice(&a.to_le_bytes());
            let select = c.to_le_bytes();
            u32::from_le_bytes([
                permute_byte(bytes, select[0]),
                permute_byte(bytes, select[1]),
                permute_byte(bytes, select[2]),
                permute_byte(bytes, select[3]),
            ])
        }
        TernaryOp::CubeFace => {
            let (x, y, z) = (f(a), f(b), f(c));
            let face: f32 = match cube_axis(x, y, z) {
                2 => {
                    if z < 0.0 {
                        5.0
                    } else {
                        4.0
                    }
                }
                1 => {
                    if y < 0.0 {
                        3.0
                    } else {
                        2.0
                    }
                }
                _ => {
                    if x < 0.0 {
                        1.0
                    } else {
                        0.0
                    }
                }
            };
            face.to_bits()
        }
        TernaryOp::CubeS => {
            let (x, y, z) = (f(a), f(b), f(c));
            let s = match cube_axis(x, y, z) {
                2 => {
                    if z < 0.0 {
                        -x
                    } else {
                        x
                    }
                }
                1 => x,
                _ => {
                    if x < 0.0 {
                        z
                    } else {
                        -z
                    }
                }
            };
            s.to_bits()
        }
        TernaryOp::CubeT => {
            let (x, y, z) = (f(a), f(b), f(c));
            let t = match cube_axis(x, y, z) {
                1 => {
                    if y < 0.0 {
                        -z
                    } else {
                        z
                    }
                }
                _ => -y,
            };
            t.to_bits()
        }
        TernaryOp::CubeMajor => {
            let (x, y, z) = (f(a), f(b), f(c));
            let major = match cube_axis(x, y, z) {
                2 => z,
                1 => y,
                _ => x,
            };
            (2.0 * major).to_bits()
        }
    }
}

/// Evaluates a comparison.
// The comparisons are written out as the instruction set defines them:
// with a NaN operand "not less" is true and "less or greater" is false,
// which the shorter forms a linter suggests would get wrong.
#[allow(clippy::double_comparisons, clippy::neg_cmp_op_on_partial_ord)]
pub fn compare(op: CompareOp, a: u32, b: u32) -> bool {
    let (x, y) = (f(a), f(b));
    match op {
        CompareOp::Equal => a == b,
        CompareOp::NotEqual => a != b,
        CompareOp::UnsignedLess => a < b,
        CompareOp::UnsignedLessEqual => a <= b,
        CompareOp::UnsignedGreater => a > b,
        CompareOp::UnsignedGreaterEqual => a >= b,
        CompareOp::SignedLess => (a as i32) < (b as i32),
        CompareOp::SignedLessEqual => (a as i32) <= (b as i32),
        CompareOp::SignedGreater => (a as i32) > (b as i32),
        CompareOp::SignedGreaterEqual => (a as i32) >= (b as i32),
        CompareOp::FloatEqual => x == y,
        CompareOp::FloatLess => x < y,
        CompareOp::FloatLessEqual => x <= y,
        CompareOp::FloatGreater => x > y,
        CompareOp::FloatGreaterEqual => x >= y,
        CompareOp::FloatLessOrGreater => x < y || x > y,
        CompareOp::FloatOrdered => !x.is_nan() && !y.is_nan(),
        CompareOp::FloatUnordered => x.is_nan() || y.is_nan(),
        CompareOp::FloatNotEqual => x != y,
        CompareOp::FloatNotLess => !(x < y),
        CompareOp::FloatNotLessEqual => !(x <= y),
        CompareOp::FloatNotGreater => !(x > y),
        CompareOp::FloatNotGreaterEqual => !(x >= y),
        CompareOp::FloatNotLessOrGreater => !(x < y || x > y),
    }
}

/// Whether a float belongs to one of the classes set in `classes`; see
/// [`super::Op::FloatClass`] for the bits.
pub fn float_class(value: u32, classes: u32) -> bool {
    let negative = value >> 31 != 0;
    let exponent = value >> 23 & 0xFF;
    let fraction = value & 0x7F_FFFF;
    let class = match (exponent, fraction) {
        (0xFF, 0) => {
            if negative {
                2
            } else {
                9
            }
        }
        // The top fraction bit tells a quiet NaN from a signalling one.
        (0xFF, _) => u32::from(fraction >> 22 != 0),
        (0, 0) => {
            if negative {
                5
            } else {
                6
            }
        }
        (0, _) => {
            if negative {
                4
            } else {
                7
            }
        }
        _ => {
            if negative {
                3
            } else {
                8
            }
        }
    };
    classes >> class & 1 != 0
}

/// Whether `a + b + carry_in` overflows 32 bits.
pub fn carry(a: u32, b: u32, carry_in: bool) -> bool {
    u64::from(a) + u64::from(b) + u64::from(carry_in) > u64::from(u32::MAX)
}

/// Whether `a - b - borrow_in` goes below zero.
pub fn borrow(a: u32, b: u32, borrow_in: bool) -> bool {
    u64::from(a) < u64::from(b) + u64::from(borrow_in)
}

/// A buffer descriptor's four words, read the way the console GPU reads
/// them (design decision T6).
///
/// ```text
/// word 0            bits 0 to 31 of the title address
/// word 1  0 to 7    bits 32 to 39 of the title address
///         16 to 29  the stride in bytes
/// word 2            the number of records
/// word 3  28 to 29  how the bound is checked
/// ```
///
/// A unit of an access lies at `index * stride + offset` bytes from the
/// address. Whether it is inside the buffer goes by the bound check, in the
/// four ways AMD's open-source drivers document for this GPU generation:
///
/// - 0: outside if `index >= records` or `offset >= stride`;
/// - 1: outside if `index >= records`;
/// - 2: outside only if `records` is zero;
/// - 3: outside if the byte position `index * stride + offset` is at or
///   past `records`, which then counts bytes.
///
/// The field positions are the public ones and agree with every descriptor
/// the real title's programs build in code (a root view's descriptor has
/// stride 1, all-ones records and check 1). The four checks are taken on
/// trust; nothing here was measured on hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferView {
    /// The title address of the first byte.
    pub address: u64,
    /// The stride in bytes.
    pub stride: u32,
    /// The record count.
    pub records: u32,
    /// The bound check, 0 to 3.
    pub check: u8,
}

impl BufferView {
    /// Reads a descriptor's four words.
    pub fn from_words(words: [u32; 4]) -> BufferView {
        BufferView {
            address: u64::from(words[0]) | u64::from(words[1] & 0xFF) << 32,
            stride: words[1] >> 16 & 0x3FFF,
            records: words[2],
            check: (words[3] >> 28 & 3) as u8,
        }
    }

    /// The title address of the unit at element `index`, byte `offset`, or
    /// `None` if the unit is outside the buffer.
    pub fn locate(&self, index: u32, offset: u32) -> Option<u64> {
        let position = u64::from(index) * u64::from(self.stride) + u64::from(offset);
        let outside = match self.check {
            0 => index >= self.records || offset >= self.stride,
            1 => index >= self.records,
            2 => self.records == 0,
            _ => position >= u64::from(self.records),
        };
        (!outside).then(|| self.address.wrapping_add(position))
    }
}

/// How the bits of one channel of a buffer format are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// An unsigned integer divided by its largest value: 0 to 1.
    Unorm,
    /// A signed integer divided by its largest value: -1 to 1.
    Snorm,
    /// An unsigned integer converted to a float.
    Uscaled,
    /// A signed integer converted to a float.
    Sscaled,
    /// An unsigned integer, as it is.
    Uint,
    /// A signed integer, sign-extended.
    Sint,
    /// A float of the channel's width.
    Float,
}

/// A data format of a typed buffer access: the widths of its channels from
/// the lowest bits up (zero for a channel the format lacks) and how a
/// channel is read.
///
/// The numbers are those of the console GPU's generation, as AMD's
/// open-source drivers list them; a typed buffer instruction carries one,
/// and so does word 3 of a buffer descriptor in bits 12 to 18. That the
/// channel widths in a format's name run from the top bits down (so that
/// `10_11_11` has its 10-bit channel on top) is taken on trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferFormat {
    /// The width in bits of x, y, z and w.
    pub bits: [u8; 4],
    /// How each channel is read.
    pub kind: ChannelKind,
}

impl BufferFormat {
    /// The format with the given number, or `None` for a number that names
    /// no format here.
    pub fn from_code(code: u32) -> Option<BufferFormat> {
        use ChannelKind::{Float, Sint, Snorm, Sscaled, Uint, Unorm, Uscaled};
        const SIX: [ChannelKind; 7] = [Unorm, Snorm, Uscaled, Sscaled, Uint, Sint, Float];
        const THREE: [ChannelKind; 3] = [Uint, Sint, Float];
        // Each family: its first number, its channel widths, its kinds.
        let families: [(u32, [u8; 4], &[ChannelKind]); 14] = [
            (1, [8, 0, 0, 0], &SIX[..6]),
            (7, [16, 0, 0, 0], &SIX),
            (14, [8, 8, 0, 0], &SIX[..6]),
            (20, [32, 0, 0, 0], &THREE),
            (23, [16, 16, 0, 0], &SIX),
            (30, [11, 11, 10, 0], &SIX),
            (37, [10, 11, 11, 0], &SIX),
            (44, [2, 10, 10, 10], &SIX[..6]),
            (50, [10, 10, 10, 2], &SIX[..6]),
            (56, [8, 8, 8, 8], &SIX[..6]),
            (62, [32, 32, 0, 0], &THREE),
            (65, [16, 16, 16, 16], &SIX),
            (72, [32, 32, 32, 0], &THREE),
            (75, [32, 32, 32, 32], &THREE),
        ];
        families.iter().find_map(|&(first, bits, kinds)| {
            let index = code.checked_sub(first)? as usize;
            kinds.get(index).map(|&kind| BufferFormat { bits, kind })
        })
    }

    /// How many bytes one element of the format occupies.
    pub fn bytes(&self) -> u32 {
        self.bits.iter().map(|&bits| u32::from(bits)).sum::<u32>() / 8
    }

    /// Reads the four channels out of an element's bytes, given as a number
    /// with the first byte lowest. A channel the format lacks reads 0, and
    /// a lacking w reads one (1.0, or the integer 1 in an integer format).
    pub fn decode(&self, raw: u128) -> [u32; 4] {
        let integer = matches!(self.kind, ChannelKind::Uint | ChannelKind::Sint);
        let mut out = [0, 0, 0, if integer { 1 } else { 1f32.to_bits() }];
        let mut low = 0u32;
        for (channel, &bits) in self.bits.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            let bits = u32::from(bits);
            let code = ((raw >> low) & ((1u128 << bits) - 1)) as u32;
            low += bits;
            let signed = ((code << (32 - bits)) as i32) >> (32 - bits);
            let largest = if bits == 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            };
            out[channel] = match self.kind {
                ChannelKind::Unorm => (code as f32 / largest as f32).to_bits(),
                ChannelKind::Snorm => {
                    let largest = (largest >> 1) as f32;
                    (signed as f32 / largest).max(-1.0).to_bits()
                }
                ChannelKind::Uscaled => (code as f32).to_bits(),
                ChannelKind::Sscaled => (signed as f32).to_bits(),
                ChannelKind::Uint => code,
                ChannelKind::Sint => signed as u32,
                ChannelKind::Float => match bits {
                    32 => code,
                    16 => float_from_half(code as u16).to_bits(),
                    _ => small_float(code, bits - 5).to_bits(),
                },
            };
        }
        out
    }

    /// Packs four channels into an element's bytes, the inverse of
    /// [`BufferFormat::decode`]. Returns `None` for the formats with 10-bit
    /// and 11-bit floats, which this module does not write.
    pub fn encode(&self, values: [u32; 4]) -> Option<u128> {
        let mut raw = 0u128;
        let mut low = 0u32;
        for (channel, &bits) in self.bits.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            let bits = u32::from(bits);
            let value = values[channel];
            let field = if bits == 32 {
                u32::MAX
            } else {
                (1u32 << bits) - 1
            };
            let largest_signed = (field >> 1) as f32;
            let code = match self.kind {
                ChannelKind::Unorm => {
                    let clamped = f(value).clamp(0.0, 1.0);
                    let clamped = if clamped.is_nan() { 0.0 } else { clamped };
                    (f64::from(clamped) * f64::from(field)).round_ties_even() as u32
                }
                ChannelKind::Snorm => {
                    let clamped = f(value).clamp(-1.0, 1.0);
                    let clamped = if clamped.is_nan() { 0.0 } else { clamped };
                    (clamped * largest_signed).round_ties_even() as i32 as u32 & field
                }
                ChannelKind::Uscaled => (f(value) as u32).min(field),
                ChannelKind::Sscaled => {
                    let limit = largest_signed as i32;
                    (f(value) as i32).clamp(-limit - 1, limit) as u32 & field
                }
                ChannelKind::Uint | ChannelKind::Sint => value & field,
                ChannelKind::Float => match bits {
                    32 => value,
                    16 => u32::from(half_from_float(f(value), false)),
                    _ => return None,
                },
            };
            raw |= u128::from(code) << low;
            low += bits;
        }
        Some(raw)
    }
}

/// An unsigned float of 10 or 11 bits: 5 bits of exponent above
/// `fraction_bits` of fraction.
fn small_float(code: u32, fraction_bits: u32) -> f32 {
    let exponent = code >> fraction_bits & 0x1F;
    let fraction = code & ((1 << fraction_bits) - 1);
    let scale = (1u32 << fraction_bits) as f32;
    match (exponent, fraction) {
        (0, _) => fraction as f32 / scale * 2f32.powi(-14),
        (31, 0) => f32::INFINITY,
        (31, _) => f32::NAN,
        _ => (1.0 + fraction as f32 / scale) * 2f32.powi(exponent as i32 - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(value: f32) -> u32 {
        value.to_bits()
    }

    #[test]
    fn the_minimum_puts_minus_zero_below_plus_zero_and_drops_a_nan() {
        assert_eq!(
            binary(BinaryOp::FloatMin, bits(0.0), bits(-0.0)),
            bits(-0.0)
        );
        assert_eq!(
            binary(BinaryOp::FloatMin, bits(-0.0), bits(0.0)),
            bits(-0.0)
        );
        assert_eq!(binary(BinaryOp::FloatMax, bits(-0.0), bits(0.0)), bits(0.0));
        assert_eq!(binary(BinaryOp::FloatMax, bits(0.0), bits(-0.0)), bits(0.0));
        assert_eq!(
            binary(BinaryOp::FloatMin, bits(f32::NAN), bits(3.0)),
            bits(3.0)
        );
        assert_eq!(
            binary(BinaryOp::FloatMax, bits(3.0), bits(f32::NAN)),
            bits(3.0)
        );
        assert_eq!(binary(BinaryOp::FloatMin, bits(2.0), bits(3.0)), bits(2.0));
    }

    #[test]
    fn a_legacy_multiply_by_zero_is_zero_whatever_the_other_operand() {
        for other in [f32::INFINITY, f32::NAN, -5.0] {
            assert_eq!(binary(BinaryOp::FloatMulLegacy, bits(0.0), bits(other)), 0);
            assert_eq!(binary(BinaryOp::FloatMulLegacy, bits(other), bits(-0.0)), 0);
        }
        assert_eq!(
            binary(BinaryOp::FloatMulLegacy, bits(2.0), bits(3.0)),
            bits(6.0)
        );
    }

    #[test]
    fn conversions_to_integers_saturate_and_give_zero_for_a_nan() {
        assert_eq!(unary(UnaryOp::SignedFromFloat, bits(3e9)), i32::MAX as u32);
        assert_eq!(unary(UnaryOp::SignedFromFloat, bits(-3e9)), i32::MIN as u32);
        assert_eq!(unary(UnaryOp::SignedFromFloat, bits(f32::NAN)), 0);
        assert_eq!(unary(UnaryOp::SignedFromFloat, bits(-2.7)), -2i32 as u32);
        assert_eq!(unary(UnaryOp::UnsignedFromFloat, bits(-1.0)), 0);
        assert_eq!(unary(UnaryOp::UnsignedFromFloat, bits(5e9)), u32::MAX);
        assert_eq!(unary(UnaryOp::UnsignedFromFloat, bits(f32::NAN)), 0);
    }

    #[test]
    fn sine_and_cosine_take_turns_and_are_exact_at_the_quarters() {
        assert_eq!(unary(UnaryOp::SinTurns, bits(0.5)), bits(0.0));
        assert_eq!(unary(UnaryOp::CosTurns, bits(0.25)), bits(0.0));
        assert_eq!(unary(UnaryOp::SinTurns, bits(0.25)), bits(1.0));
        assert_eq!(unary(UnaryOp::CosTurns, bits(0.5)), bits(-1.0));
        assert_eq!(unary(UnaryOp::CosTurns, bits(-3.0)), bits(1.0));
        assert_eq!(unary(UnaryOp::SinTurns, bits(1000.0)), bits(0.0));
        assert_eq!(unary(UnaryOp::CosTurns, bits(1000.0)), bits(1.0));
        let eighth = f32::from_bits(unary(UnaryOp::SinTurns, bits(0.125)));
        assert!((eighth - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn the_fraction_never_reaches_one() {
        assert_eq!(unary(UnaryOp::Fract, bits(-1e-10)), 0x3F7F_FFFF);
        assert_eq!(unary(UnaryOp::Fract, bits(2.75)), bits(0.75));
        assert_eq!(unary(UnaryOp::Fract, bits(-0.25)), bits(0.75));
    }

    #[test]
    fn the_median_of_three_with_a_nan_is_the_minimum() {
        let median = |a: f32, b: f32, c: f32| {
            f32::from_bits(ternary(TernaryOp::FloatMedian, bits(a), bits(b), bits(c)))
        };
        assert_eq!(median(1.0, 3.0, 2.0), 2.0);
        assert_eq!(median(3.0, 1.0, 2.0), 2.0);
        assert_eq!(median(2.0, 3.0, 1.0), 2.0);
        assert_eq!(median(f32::NAN, 3.0, 2.0), 2.0);
        assert_eq!(median(5.0, 0.0, 1.0), 1.0);
    }

    #[test]
    fn halves_round_to_nearest_even_or_toward_zero() {
        assert_eq!(half_from_float(1.0, false), 0x3C00);
        assert_eq!(half_from_float(-2.0, false), 0xC000);
        assert_eq!(half_from_float(65504.0, false), 0x7BFF);
        assert_eq!(half_from_float(1e9, false), 0x7C00);
        assert_eq!(half_from_float(1e9, true), 0x7BFF);
        assert_eq!(half_from_float(f32::INFINITY, true), 0x7C00);
        // 1 + 3/4096 lies between two halves, nearer the upper one.
        let between = 1.0 + 3.0 / 4096.0;
        assert_eq!(half_from_float(between, false), 0x3C01);
        assert_eq!(half_from_float(between, true), 0x3C00);
        // Exactly halfway goes to the even neighbour.
        assert_eq!(half_from_float(1.0 + 1.0 / 2048.0, false), 0x3C00);
        assert_eq!(half_from_float(1.0 + 3.0 / 2048.0, false), 0x3C02);
        // The smallest denormal half, and half of it, which rounds to even.
        assert_eq!(half_from_float(2f32.powi(-24), false), 0x0001);
        assert_eq!(half_from_float(2f32.powi(-25), false), 0x0000);
        assert_eq!(half_from_float(1.5 * 2f32.powi(-24), false), 0x0002);
        assert!(float_from_half(half_from_float(f32::NAN, false)).is_nan());
        for half in [
            0x0000u16, 0x0001, 0x03FF, 0x0400, 0x3C00, 0x7BFF, 0x7C00, 0x8001, 0xFBFF,
        ] {
            assert_eq!(half_from_float(float_from_half(half), false), half);
            assert_eq!(half_from_float(float_from_half(half), true), half);
        }
    }

    #[test]
    fn bytes_are_permuted_with_the_second_operand_as_the_low_four() {
        let value = ternary(
            TernaryOp::PermuteBytes,
            0x8877_6655,
            0x4433_22F1,
            0x0D0C_0400,
        );
        // Byte 0 picks byte 0 (0xF1), byte 1 picks byte 4 (0x55), byte 2
        // gives 0x00, byte 3 gives 0xFF.
        assert_eq!(value, 0xFF00_55F1);
        // 8 repeats the sign of byte 1 (0x22: clear), 11 that of byte 7
        // (0x88: set).
        assert_eq!(
            ternary(
                TernaryOp::PermuteBytes,
                0x8877_6655,
                0x4433_22F1,
                0x0C0C_0B08
            ),
            0x0000_FF00
        );
    }

    #[test]
    fn a_cube_direction_picks_its_face_by_the_largest_component() {
        let face = |x: f32, y: f32, z: f32| {
            f32::from_bits(ternary(TernaryOp::CubeFace, bits(x), bits(y), bits(z)))
        };
        assert_eq!(face(1.0, 0.0, 0.0), 0.0);
        assert_eq!(face(-1.0, 0.5, 0.0), 1.0);
        assert_eq!(face(0.0, 2.0, 1.0), 2.0);
        assert_eq!(face(0.0, -2.0, 1.0), 3.0);
        assert_eq!(face(1.0, 1.0, 1.0), 4.0);
        assert_eq!(face(0.0, 0.0, -1.0), 5.0);
        assert_eq!(
            ternary(TernaryOp::CubeMajor, bits(0.5), bits(-3.0), bits(1.0)),
            bits(-6.0)
        );
        assert_eq!(
            ternary(TernaryOp::CubeS, bits(-2.0), bits(0.5), bits(1.0)),
            bits(1.0)
        );
        assert_eq!(
            ternary(TernaryOp::CubeT, bits(0.25), bits(-2.0), bits(1.0)),
            bits(-1.0)
        );
    }

    #[test]
    fn float_classes_are_told_apart() {
        assert!(float_class(bits(f32::INFINITY), 1 << 9));
        assert!(float_class(bits(f32::NEG_INFINITY), 1 << 2));
        assert!(float_class(bits(f32::NAN), 0b11));
        assert!(float_class(bits(-0.0), 1 << 5));
        assert!(float_class(bits(0.0), 1 << 6));
        assert!(float_class(1, 1 << 7));
        assert!(float_class(bits(-3.0), 1 << 3));
        assert!(!float_class(bits(3.0), 0x2FF));
    }

    #[test]
    fn a_buffer_view_checks_its_bound_in_the_way_its_descriptor_says() {
        let view = |stride: u32, records: u32, check: u32| {
            BufferView::from_words([0x1000, 0x12 | stride << 16, records, check << 28])
        };
        assert_eq!(view(16, 4, 0).address, 0x12_0000_1000);
        // Structured with offset: the index and the offset are both bound.
        assert_eq!(view(16, 4, 0).locate(3, 12), Some(0x12_0000_1000 + 60));
        assert_eq!(view(16, 4, 0).locate(4, 0), None);
        assert_eq!(view(16, 4, 0).locate(0, 16), None);
        // Index only.
        assert_eq!(view(1, u32::MAX, 1).locate(0, 5000), Some(0x12_0000_2388));
        assert_eq!(view(16, 4, 1).locate(4, 0), None);
        // Disabled unless empty.
        assert_eq!(view(4, 1, 2).locate(9, 0), Some(0x12_0000_1024));
        assert_eq!(view(4, 0, 2).locate(0, 0), None);
        // Raw: the byte position against a byte count.
        assert_eq!(view(0, 8, 3).locate(0, 7), Some(0x12_0000_1007));
        assert_eq!(view(0, 8, 3).locate(0, 8), None);
    }

    #[test]
    fn buffer_formats_are_read_by_number_and_decode_their_channels() {
        // 8_8_8_8_UNORM: 0, 255, 51 and 255 over 255.
        let rgba = BufferFormat::from_code(56).unwrap();
        assert_eq!(rgba.bytes(), 4);
        assert_eq!(
            rgba.decode(0xFF33_FF00),
            [bits(0.0), bits(1.0), bits(0.2), bits(1.0)]
        );
        // 16_16_FLOAT lacks z and w: they read 0 and 1.
        let half2 = BufferFormat::from_code(29).unwrap();
        assert_eq!(
            half2.decode(0xC000_3C00),
            [bits(1.0), bits(-2.0), 0, bits(1.0)]
        );
        // 32_32_32_UINT lacks w, which reads the integer 1.
        let uint3 = BufferFormat::from_code(72).unwrap();
        assert_eq!(uint3.bytes(), 12);
        assert_eq!(uint3.decode(7 | 8 << 32 | 9 << 64), [7, 8, 9, 1]);
        // 8_8_SNORM: -128 and -127 both read -1.
        let snorm = BufferFormat::from_code(15).unwrap();
        assert_eq!(snorm.decode(0x8180), [bits(-1.0), bits(-1.0), 0, bits(1.0)]);
        // 16_SINT sign-extends.
        let sint = BufferFormat::from_code(12).unwrap();
        assert_eq!(sint.decode(0xFFFE), [-2i32 as u32, 0, 0, 1]);
        // 2_10_10_10 has its 2-bit channel on top, as w.
        let packed = BufferFormat::from_code(50).unwrap();
        assert_eq!(packed.bits, [10, 10, 10, 2]);
        assert_eq!(
            packed.decode(0xC000_03FF),
            [bits(1.0), bits(0.0), bits(0.0), bits(1.0)]
        );
        assert!(BufferFormat::from_code(0).is_none());
        assert!(BufferFormat::from_code(13).is_some());
        assert!(BufferFormat::from_code(78).is_none());
    }

    #[test]
    fn encoding_a_buffer_format_undoes_decoding_it() {
        for code in [5, 12, 20, 22, 29, 56, 57, 61, 64, 70, 71, 77] {
            let format = BufferFormat::from_code(code).unwrap();
            let raw = 0x8040_C020_1234_FFFE_7F01_80FF_3C00_0001u128
                & (u128::MAX >> (128 - 8 * format.bytes()));
            let decoded = format.decode(raw);
            let again = format.encode(decoded).unwrap();
            assert_eq!(format.decode(again), decoded, "format {code}");
        }
    }
}
