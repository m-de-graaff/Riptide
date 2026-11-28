//! The intermediate form: a program as one lane sees it.
//!
//! The console GPU runs a program on a wave of 32 or 64 lanes at once, with
//! a mask saying which lanes are live. A host shader runs once per lane. The
//! intermediate form is therefore written per lane (decision T2 of
//! `docs/design/2026-11-28-shader-translation.md`): a [`Function`] is what
//! one lane does.
//!
//! # Shape
//!
//! A function is a control-flow graph of [`Block`]s. A block is a list of
//! [`Inst`]s and ends in a [`Terminator`]. Values are 32-bit words
//! ([`Ty::Word`]) or per-lane booleans ([`Ty::Flag`]); a 64-bit address is a
//! pair of words, low then high ([`Address`]).
//!
//! State that lives across blocks is held in variables ([`Var`]): one for
//! each machine register the program uses, read with [`Op::Get`] and written
//! with [`Op::Set`]. Everything else is a temporary ([`Temp`]): defined once
//! by one instruction, and used only later in the same block. This is the
//! form compilers call "registers in memory": an emitter declares every
//! variable as a local of the host shader and every temporary as a constant,
//! and no merging of values at joins is ever needed. All variables start as
//! zero (false), except [`Var::Exec`], which starts true.
//!
//! # How a wave becomes a lane
//!
//! - A vector register is a word variable of the lane ([`Var::Vector`]).
//! - A scalar register holds a number that the whole wave shares
//!   ([`Var::Scalar`]), or a lane mask. A lane mask becomes a flag: this
//!   lane's bit of it ([`Var::Mask`], [`Var::Exec`], [`Var::Vcc`]). The lift
//!   decides which of the two each write produces.
//! - A wave-level branch becomes a branch of the lane on its own bit, as
//!   section 8 of the research note describes. The live-lane mask `EXEC` is
//!   the flag [`Var::Exec`]; a vector write under a mask that may be clear
//!   is a [`Op::Select`] between the new and the old value, and an effect
//!   (a store, an export) carries a `guard` flag.
//! - What looks across lanes is an explicit operation, so that an emitter
//!   can map it to a host wave intrinsic or refuse it: [`Op::FirstLane`],
//!   [`Op::ReadLane`], [`Op::CountLanes`], [`Op::FirstLaneIndex`],
//!   [`Op::AnyLane`], [`Op::LanesBelow`], [`Op::BallotWord`],
//!   [`Op::LaneBit`], [`Op::QuadPermute`], [`Op::QuadAny`]. Their meaning is
//!   over "the lanes that reach the operation together", which is what a
//!   host's wave intrinsics mean too. [`crate::interp`] defines it exactly.
//!
//! # Control flow
//!
//! [`cfg::ControlFlow`] computes what an emitter of structured code needs:
//! predecessors, a reverse post-order, immediate dominators, back edges,
//! loop headers with their bodies, and whether the graph is reducible. The
//! lift only hands out reducible graphs: [`cfg::make_reducible`] splits
//! nodes until the graph is.

pub mod cfg;
pub mod eval;
mod text;

use crate::container::xbox::HardwareStage;
use crate::gfx10::WaveSize;

/// The index of a block in [`Function::blocks`].
pub type BlockId = usize;

/// A temporary: a value defined by one instruction and used only later in
/// the same block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Temp(pub u32);

/// The type of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ty {
    /// A 32-bit word. Whether it is an integer or the bits of a float is up
    /// to the operation that reads it.
    Word,
    /// A boolean of the lane.
    Flag,
}

/// An operand of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Value {
    /// The result of an earlier instruction of the same block.
    Temp(Temp),
    /// A constant word.
    Word(u32),
    /// A constant flag.
    Flag(bool),
}

impl Value {
    /// The bits of a float constant.
    pub fn float(value: f32) -> Value {
        Value::Word(value.to_bits())
    }

    /// The constant word, if the value is one.
    pub fn as_word(self) -> Option<u32> {
        match self {
            Value::Word(word) => Some(word),
            _ => None,
        }
    }

    /// The constant flag, if the value is one.
    pub fn as_flag(self) -> Option<bool> {
        match self {
            Value::Flag(flag) => Some(flag),
            _ => None,
        }
    }
}

/// A variable: state of the lane that lives across blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Var {
    /// Vector register `v<n>`: a word of the lane.
    Vector(u8),
    /// Scalar register `s<n>` read as a number. Every lane of a wave holds
    /// the same number here, as long as the program is one a compiler made
    /// (see the module documentation of [`crate::lift`]).
    Scalar(u8),
    /// The lane's bit of the lane mask held in scalar register `s<n>` (32
    /// lanes) or in the pair `s<n>`, `s<n+1>` (64 lanes).
    Mask(u8),
    /// The lane's bit of `EXEC`: whether the lane is live. Starts true.
    Exec,
    /// The lane's bit of `VCC`, the vector condition code.
    Vcc,
    /// The scalar condition code. After an operation on numbers it is the
    /// same in every lane. After an operation on lane masks the console's
    /// flag says "some lane is set" and this flag is the lane's own bit.
    Scc,
    /// Register `M0` as a number.
    M0,
    /// Whether a pixel program has passed its final export. A lane that ends
    /// without it is discarded.
    Exported,
}

impl Var {
    /// The type of the variable.
    pub fn ty(self) -> Ty {
        match self {
            Var::Vector(_) | Var::Scalar(_) | Var::M0 => Ty::Word,
            Var::Mask(_) | Var::Exec | Var::Vcc | Var::Scc | Var::Exported => Ty::Flag,
        }
    }
}

/// A 64-bit title address as two words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address {
    /// Bits 0 to 31.
    pub low: Value,
    /// Bits 32 to 63.
    pub high: Value,
}

/// An operation on one word. Float operations take and give the bits of a
/// 32-bit float. Where the console's result differs from what a host
/// intrinsic of the same name gives, the difference is stated; an emitter has
/// to reproduce it. [`eval::unary`] is the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// Bitwise complement.
    Not,
    /// Number of set bits.
    CountBits,
    /// Index of the lowest set bit, or all ones if the word is zero.
    FindLowBit,
    /// Number of zero bits above the highest set bit, or all ones if the
    /// word is zero.
    LeadingZeros,
    /// For a signed word: number of bits above the highest bit that differs
    /// from the sign bit, or all ones if no bit differs (0 and -1).
    LeadingSignBits,
    /// The bits in reverse order.
    ReverseBits,
    /// Clamp a float to 0..1; a NaN gives 0. The `clamp` modifier of float
    /// operations.
    Saturate,
    /// 1 / x.
    Reciprocal,
    /// 1 / sqrt(x).
    ReciprocalSqrt,
    /// sqrt(x).
    Sqrt,
    /// 2 to the power x.
    Exp2,
    /// Logarithm to base 2.
    Log2,
    /// Sine of an angle given in turns (x times 2 pi radians). An input
    /// outside -256..256 gives 0. A half turn gives exactly +0.
    SinTurns,
    /// Cosine of an angle given in turns. An input outside -256..256 gives
    /// 1. A quarter turn gives exactly +0.
    CosTurns,
    /// Round toward minus infinity.
    Floor,
    /// Round toward plus infinity.
    Ceil,
    /// Round toward zero.
    Trunc,
    /// Round to nearest, ties to even.
    RoundEven,
    /// x - floor(x), but never 1.0: a result that would round up to 1.0 is
    /// the largest float below it.
    Fract,
    /// Signed integer to float, rounded to nearest even.
    FloatFromSigned,
    /// Unsigned integer to float, rounded to nearest even.
    FloatFromUnsigned,
    /// Float to signed integer toward zero. Saturates; a NaN gives 0.
    SignedFromFloat,
    /// Float to unsigned integer toward zero. Saturates; negative values and
    /// a NaN give 0.
    UnsignedFromFloat,
    /// A 32-bit float to the bits of a 16-bit float in the low half of the
    /// word (high half zero), rounded to nearest even.
    HalfFromFloat,
    /// The same, rounded toward zero.
    HalfFromFloatTowardZero,
    /// The 16-bit float in the low half of the word to a 32-bit float.
    FloatFromHalf,
}

/// An operation on two words. [`eval::binary`] is the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// Wrapping addition.
    Add,
    /// Wrapping subtraction.
    Sub,
    /// The low 32 bits of the product.
    Mul,
    /// The high 32 bits of the unsigned product.
    MulHighUnsigned,
    /// The high 32 bits of the signed product.
    MulHighSigned,
    /// Bitwise and.
    And,
    /// Bitwise or.
    Or,
    /// Bitwise exclusive or.
    Xor,
    /// Shift left by the low 5 bits of the second word.
    ShiftLeft,
    /// Shift right, filling with zeroes, by the low 5 bits of the second
    /// word.
    ShiftRight,
    /// Shift right, filling with the sign bit, by the low 5 bits of the
    /// second word.
    ShiftRightSigned,
    /// The smaller as unsigned numbers.
    MinUnsigned,
    /// The larger as unsigned numbers.
    MaxUnsigned,
    /// The smaller as signed numbers.
    MinSigned,
    /// The larger as signed numbers.
    MaxSigned,
    /// Float addition.
    FloatAdd,
    /// Float subtraction.
    FloatSub,
    /// Float multiplication.
    FloatMul,
    /// Float multiplication where zero times anything (infinity and NaN
    /// included) is +0. The console's `v_mul_legacy_f32`.
    FloatMulLegacy,
    /// The smaller float. A NaN operand gives the other operand; -0 is
    /// smaller than +0.
    FloatMin,
    /// The larger float. A NaN operand gives the other operand; +0 is larger
    /// than -0.
    FloatMax,
    /// The first float times 2 to the power of the second word as a signed
    /// integer.
    FloatScale,
}

/// An operation on three words. [`eval::ternary`] is the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TernaryOp {
    /// a * b + c as floats with one rounding (fused).
    FloatFused,
    /// a * b + c as floats with the product rounded before the sum.
    FloatMulAdd,
    /// The median of three floats. With a NaN among them, the minimum of
    /// the three by the rule of [`BinaryOp::FloatMin`].
    FloatMedian,
    /// Byte permutation, the console's `v_perm_b32`. Byte i of the result is
    /// picked by byte i of `c` from the eight bytes of `a` and `b`, with `b`
    /// the low four: 0 to 7 pick a byte; 8, 9, 10, 11 give 0xFF or 0x00 by
    /// the top bit of byte 1, 3, 5, 7; 12 gives 0x00; anything higher 0xFF.
    PermuteBytes,
    /// The face of a cube map a direction (a, b, c) = (x, y, z) points at,
    /// as a float 0 to 5 (+x, -x, +y, -y, +z, -z). The major axis is z when
    /// |z| >= |x| and |z| >= |y|, else y when |y| >= |x|, else x.
    CubeFace,
    /// The s coordinate on that face before division by the major axis.
    CubeS,
    /// The t coordinate on that face before division by the major axis.
    CubeT,
    /// Twice the component along the major axis.
    CubeMajor,
}

/// A comparison of two words, giving a flag. [`eval::compare`] is the
/// reference. The float comparisons named `Not…` are true when an operand is
/// a NaN; the others are false then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompareOp {
    /// Equal words.
    Equal,
    /// Different words.
    NotEqual,
    /// Unsigned less than.
    UnsignedLess,
    /// Unsigned less than or equal.
    UnsignedLessEqual,
    /// Unsigned greater than.
    UnsignedGreater,
    /// Unsigned greater than or equal.
    UnsignedGreaterEqual,
    /// Signed less than.
    SignedLess,
    /// Signed less than or equal.
    SignedLessEqual,
    /// Signed greater than.
    SignedGreater,
    /// Signed greater than or equal.
    SignedGreaterEqual,
    /// Float equal.
    FloatEqual,
    /// Float less than.
    FloatLess,
    /// Float less than or equal.
    FloatLessEqual,
    /// Float greater than.
    FloatGreater,
    /// Float greater than or equal.
    FloatGreaterEqual,
    /// Float less than or greater than (ordered and not equal).
    FloatLessOrGreater,
    /// Neither operand is a NaN.
    FloatOrdered,
    /// An operand is a NaN.
    FloatUnordered,
    /// Not (a == b).
    FloatNotEqual,
    /// Not (a < b).
    FloatNotLess,
    /// Not (a <= b).
    FloatNotLessEqual,
    /// Not (a > b).
    FloatNotGreater,
    /// Not (a >= b).
    FloatNotGreaterEqual,
    /// Not (a < b or a > b).
    FloatNotLessOrGreater,
}

/// An operation on two flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlagOp {
    /// Both.
    And,
    /// Either.
    Or,
    /// Exactly one.
    Xor,
}

/// A pair of barycentric weights a pixel program is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weights {
    /// Perspective-correct, at the sample.
    PerspectiveSample,
    /// Perspective-correct, at the pixel centre.
    PerspectiveCenter,
    /// Perspective-correct, at the centroid.
    PerspectiveCentroid,
    /// Linear in screen space, at the sample.
    LinearSample,
    /// Linear in screen space, at the pixel centre.
    LinearCenter,
    /// Linear in screen space, at the centroid.
    LinearCentroid,
}

/// Which of an attribute's per-vertex values an interpolation reads. The
/// hardware evaluates an attribute as P0 + i * P10 + j * P20.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VertexTerm {
    /// The value at the provoking vertex.
    P0,
    /// The value at vertex 1 minus the value at vertex 0.
    P10,
    /// The value at vertex 2 minus the value at vertex 0.
    P20,
}

/// Something the stage hands a lane from outside the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Input {
    /// User-data word n: what the root signature's parameters put in the
    /// stage's registers (design decision T7).
    UserData(u8),
    /// Vertex: the vertex's index.
    VertexId,
    /// Vertex: the instance's index.
    InstanceId,
    /// Vertex: one component of an attribute, as the 32 bits the input
    /// assembler delivers for row `row` of the input signature.
    VertexAttribute {
        /// The register (row) of `ISG1`.
        row: u8,
        /// The component, 0 for x.
        channel: u8,
    },
    /// Pixel: weight i (`second` false) or j (`second` true) of a pair, as
    /// float bits. i weighs vertex 1 and j vertex 2.
    Weight {
        /// Which pair.
        weights: Weights,
        /// False for i, true for j.
        second: bool,
    },
    /// Pixel: one component of attribute `attribute` at a vertex, or the
    /// difference of two vertices, as float bits. Row and mask of the
    /// attribute are in [`Interface::pixel_inputs`].
    Attribute {
        /// The attribute number.
        attribute: u8,
        /// The component, 0 for x.
        channel: u8,
        /// Which per-vertex term.
        term: VertexTerm,
    },
    /// Pixel: attribute `attribute`, component `channel`, interpolated with
    /// the given weights as the host's rasteriser would. The lift uses this
    /// where a program interpolates with a pair of weights exactly as it was
    /// handed them, which lets an emitter declare an ordinary interpolated
    /// input.
    Interpolated {
        /// The attribute number.
        attribute: u8,
        /// The component, 0 for x.
        channel: u8,
        /// Which weights.
        weights: Weights,
    },
    /// Pixel: the position, component 0 to 3: x and y in pixels, depth, and
    /// one over w, as float bits.
    Position(u8),
    /// Pixel: non-zero when the primitive faces front.
    FrontFace,
    /// Pixel: the console's ancillary word. Bits 8 to 11 hold the sample
    /// number.
    Ancillary,
    /// Pixel: the mask of covered samples.
    SampleCoverage,
    /// Pixel: the position as integers, x in the low 16 bits and y in the
    /// high.
    FixedPosition,
    /// Compute: the thread's position in its group along an axis.
    ThreadId(u8),
    /// Compute: the group's index along an axis.
    GroupId(u8),
}

/// Where an export goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExportTarget {
    /// A colour target, 0 to 7: four raw 32-bit values after the patch of
    /// [`crate::container::stage::ExportFormat::Abgr32`].
    Color(u8),
    /// The depth target: channel 0 depth, 1 stencil reference, 2 sample
    /// mask.
    Depth,
    /// Position n: 0 is the clip-space position; 1 and up carry clip and
    /// cull distances.
    Position(u8),
    /// Parameter n, which [`Interface::outputs`] maps to a signature row.
    Parameter(u8),
}

/// The kind of an image a sampling or load instruction addresses. The
/// instruction states it; the descriptor's own kind is not consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dimension {
    /// One coordinate.
    One,
    /// Two coordinates.
    Two,
    /// Three coordinates.
    Three,
    /// Three coordinates: s and t on a face and the face number, as
    /// [`TernaryOp::CubeFace`] and its companions compute them.
    Cube,
    /// A coordinate and a slice.
    OneArray,
    /// Two coordinates and a slice.
    TwoArray,
    /// Two coordinates and a sample number.
    TwoMultisample,
    /// Two coordinates, a slice and a sample number.
    TwoMultisampleArray,
}

impl Dimension {
    /// Reads the dimension field of an image instruction.
    pub fn from_code(code: u8) -> Dimension {
        match code & 7 {
            0 => Dimension::One,
            1 => Dimension::Two,
            2 => Dimension::Three,
            3 => Dimension::Cube,
            4 => Dimension::OneArray,
            5 => Dimension::TwoArray,
            6 => Dimension::TwoMultisample,
            _ => Dimension::TwoMultisampleArray,
        }
    }

    /// How many coordinates an access takes.
    pub fn coordinates(self) -> usize {
        match self {
            Dimension::One => 1,
            Dimension::Two | Dimension::OneArray => 2,
            Dimension::Three
            | Dimension::Cube
            | Dimension::TwoArray
            | Dimension::TwoMultisample => 3,
            Dimension::TwoMultisampleArray => 4,
        }
    }

    /// How many components each of the two gradients has.
    pub fn gradient_components(self) -> usize {
        match self {
            Dimension::One | Dimension::OneArray => 1,
            Dimension::Three => 3,
            _ => 2,
        }
    }
}

/// How a sample chooses its level of detail.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Level {
    /// From the derivatives of the coordinates across the lane's quad.
    Implicit,
    /// Level zero.
    Zero,
    /// The given level, a float.
    Explicit(Value),
    /// From the given gradients: all components of d/dx, then of d/dy.
    Gradients(Vec<Value>, Vec<Value>),
}

/// The sampler of a sampling operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sampler {
    /// A sampler of the host's sampler heap, at the place given by bits 0
    /// to 11 of word 3 of the 4-word sampler descriptor (design decision
    /// T6). The value is that place.
    Heap(Value),
    /// A sampler the root signature fixes. The program builds its four
    /// descriptor words from constants, and these are the words, in the
    /// console GPU's own layout (section 3 of the research note).
    Immediate([u32; 4]),
}

/// What an image operation does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageKind {
    /// Filtered sample through a sampler. Four results: the channels.
    Sample,
    /// The four texels a bilinear sample would blend, one channel of each.
    /// Four results in the hardware's order: (u0,v1) (u1,v1) (u1,v0)
    /// (u0,v0).
    Gather {
        /// Which channel, 0 to 3.
        channel: u8,
    },
    /// One texel by integer coordinates. Four results: the channels.
    Load,
    /// Writes one texel by integer coordinates. No result.
    Store {
        /// The four channels to write.
        values: [Value; 4],
        /// Whether the lane writes at all.
        guard: Value,
    },
    /// The image's size at the level given as the single coordinate. Four
    /// results: width, height, depth or slice count, level count.
    Info,
    /// The level of detail a sample at the coordinates would use, from the
    /// derivatives across the lane's quad. Four results: the level after
    /// clamping to the levels the image has, the level before, and two
    /// zeroes; the first two are floats.
    QueryLevel,
}

/// An image operation with its operands. The fields are in the order the
/// machine instruction's address registers hold them: offset, bias,
/// reference, gradients, coordinates, then level, clamp or mip.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageOp {
    /// What is done.
    pub kind: ImageKind,
    /// The kind of image the instruction addresses.
    pub dimension: Dimension,
    /// The texture's place in the host's resource heap: word 5 of the
    /// 8-word image descriptor (design decision T6).
    pub texture: Value,
    /// Word 6 of the image descriptor: bit 0 set for an integer format, bit
    /// 1 set if it is signed. Decides how the four channel words are read
    /// and written.
    pub format: Value,
    /// For sampling and gathering: the sampler.
    pub sampler: Option<Sampler>,
    /// Whether the access compares with `reference`. The host then needs
    /// the comparing form of the sampler (its place plus 1,024).
    pub compare: bool,
    /// The texel offset word: x in bits 0 to 5, y in bits 8 to 13, z in
    /// bits 16 to 21, each a signed 6-bit number.
    pub offset: Option<Value>,
    /// The level-of-detail bias, a float.
    pub bias: Option<Value>,
    /// The value compared with, a float.
    pub reference: Option<Value>,
    /// The coordinates, [`Dimension::coordinates`] of them: floats for
    /// sampling and gathering, integers for loads and stores.
    pub coordinates: Vec<Value>,
    /// How the level is chosen (sampling and gathering only).
    pub level: Level,
    /// The lowest level a sample may use, a float.
    pub level_clamp: Option<Value>,
    /// For loads and stores: the mip level, an integer.
    pub mip: Option<Value>,
    /// Whether sampling coordinates are in texels instead of 0..1.
    pub unnormalized: bool,
}

/// How a buffer access forms its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferPosition {
    /// The element index, multiplied by the descriptor's stride.
    pub index: Value,
    /// The byte offset added to that.
    pub offset: Value,
}

/// One operation. `results` in [`Inst`] says how many temporaries it
/// defines; the documentation of each variant says how many that is and of
/// what type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Op {
    /// Reads a variable. One result of the variable's type.
    Get(Var),
    /// Writes a variable. No result.
    Set(Var, Value),
    /// One word result.
    Unary(UnaryOp, Value),
    /// One word result.
    Binary(BinaryOp, Value, Value),
    /// One word result.
    Ternary(TernaryOp, Value, Value, Value),
    /// One flag result.
    Compare(CompareOp, Value, Value),
    /// Whether a float belongs to one of the classes whose bits are set in
    /// `classes`: bit 0 signalling NaN, 1 quiet NaN, 2 minus infinity, 3
    /// negative normal, 4 negative denormal, 5 minus zero, 6 plus zero, 7
    /// positive denormal, 8 positive normal, 9 plus infinity. One flag
    /// result.
    FloatClass {
        /// The float.
        value: Value,
        /// The set of classes.
        classes: Value,
    },
    /// `if_true` when the flag `condition` holds, else `if_false`. The two
    /// are both words or both flags. One result of their type.
    Select {
        /// The flag.
        condition: Value,
        /// The value when it holds.
        if_true: Value,
        /// The value when it does not.
        if_false: Value,
    },
    /// The negation of a flag. One flag result.
    FlagNot(Value),
    /// One flag result.
    Flag(FlagOp, Value, Value),
    /// Whether `a + b + carry_in` overflows 32 bits. `carry_in` is a flag.
    /// One flag result.
    Carry {
        /// The first word.
        a: Value,
        /// The second word.
        b: Value,
        /// The carry in.
        carry_in: Value,
    },
    /// Whether `a - b - borrow_in` goes below zero. `borrow_in` is a flag.
    /// One flag result.
    Borrow {
        /// The word subtracted from.
        a: Value,
        /// The word subtracted.
        b: Value,
        /// The borrow in.
        borrow_in: Value,
    },
    /// A stage input. One word result.
    Input(Input),
    /// Reads title memory: `words` units of `width` bits each, the first at
    /// `address` and each next one `width / 8` bytes on, as one read whose
    /// address does not wrap at 32 bits. Narrow units are zero-extended.
    /// Memory that is not mapped reads as zero. `words` word results.
    LoadMemory {
        /// The address of the first unit.
        address: Address,
        /// 8, 16 or 32.
        width: u8,
        /// How many units.
        words: u8,
    },
    /// Writes title memory: each of `values` as a unit of `width` bits, the
    /// first at `address`. Nothing is written unless `guard` holds, and a
    /// write to memory that is not mapped is dropped. No result.
    StoreMemory {
        /// The address of the first unit.
        address: Address,
        /// 8, 16 or 32.
        width: u8,
        /// The units, low bits used.
        values: Vec<Value>,
        /// Whether the lane writes at all.
        guard: Value,
    },
    /// Reads title memory through a buffer descriptor given as its four
    /// words. See [`eval::BufferView`] for how the descriptor gives the
    /// address and the bound. A unit outside the bound reads as zero.
    /// `words` word results.
    LoadBuffer {
        /// The four words of the descriptor.
        descriptor: [Value; 4],
        /// Where in the buffer.
        position: BufferPosition,
        /// 8, 16 or 32.
        width: u8,
        /// How many units.
        words: u8,
    },
    /// Writes title memory through a buffer descriptor. A unit outside the
    /// bound is dropped. No result.
    StoreBuffer {
        /// The four words of the descriptor.
        descriptor: [Value; 4],
        /// Where in the buffer.
        position: BufferPosition,
        /// 8, 16 or 32.
        width: u8,
        /// The units, low bits used.
        values: Vec<Value>,
        /// Whether the lane writes at all.
        guard: Value,
    },
    /// Reads one element of a typed buffer through a descriptor and
    /// converts it by a data format ([`eval::BufferFormat`]). `format` is
    /// the format's number: a constant when the instruction states it, or
    /// bits 12 to 18 of the descriptor's word 3 when the descriptor does.
    /// An element outside the bound reads as zeroes in every channel. Four
    /// word results: x, y, z, w.
    LoadFormat {
        /// The four words of the descriptor.
        descriptor: [Value; 4],
        /// Where in the buffer.
        position: BufferPosition,
        /// The format's number.
        format: Value,
    },
    /// Writes one element of a typed buffer, converting four channels by a
    /// data format. No result.
    StoreFormat {
        /// The four words of the descriptor.
        descriptor: [Value; 4],
        /// Where in the buffer.
        position: BufferPosition,
        /// The format's number.
        format: Value,
        /// The channels x, y, z, w.
        values: [Value; 4],
        /// Whether the lane writes at all.
        guard: Value,
    },
    /// Reads words of the program's own code, where compilers keep tables of
    /// constants. `offset` is a byte offset into [`Function::code`]; words
    /// past the end read as zero. `words` word results.
    LoadCode {
        /// The byte offset of the first word.
        offset: Value,
        /// How many words.
        words: u8,
    },
    /// An image access. Four word results, or none for a store.
    Image(Box<ImageOp>),
    /// Hands values to the next stage or to a render target. A channel that
    /// is `None` is not exported. Nothing is exported unless `guard` holds.
    /// No result.
    Export {
        /// Where the values go.
        target: ExportTarget,
        /// The four channels.
        values: [Option<Value>; 4],
        /// Whether the lane exports at all.
        guard: Value,
    },
    /// Pixel: throws the lane's pixel away if the flag holds. The lane goes
    /// on running (its derivatives are still wanted). No result.
    Discard(Value),

    /// Cross-lane: `value` of the first lane (lowest lane number) whose
    /// `active` flag holds, among the lanes that reach this operation
    /// together; if none does, of the first of those lanes. One word result.
    FirstLane {
        /// The word read.
        value: Value,
        /// Which lanes are candidates.
        active: Value,
    },
    /// Cross-lane: `value` of the lane whose number in the console's wave is
    /// `lane`; zero if that lane does not reach this operation. One word
    /// result.
    ReadLane {
        /// The word read.
        value: Value,
        /// The lane number.
        lane: Value,
    },
    /// Cross-lane: how many lanes hold the flag. One word result.
    CountLanes(Value),
    /// Cross-lane: the console lane number of the first lane that holds the
    /// flag, or all ones if none does. One word result.
    FirstLaneIndex(Value),
    /// Cross-lane: whether any lane holds the flag. One flag result.
    AnyLane(Value),
    /// Cross-lane: how many lanes with a lower console lane number hold the
    /// flag. `half` restricts the count to the console's lanes 0 to 31
    /// (`Some(false)`) or 32 to 63 (`Some(true)`), which is how the two
    /// instructions of a 64-lane program split it. One word result.
    LanesBelow {
        /// The flag counted.
        mask: Value,
        /// The half of the wave counted, or all of it.
        half: Option<bool>,
    },
    /// Cross-lane: the flags of all lanes as a number, bit n for console
    /// lane n. `high` picks bits 32 to 63. Exact only where host lanes are
    /// the console's lanes. One word result.
    BallotWord {
        /// The flag gathered.
        mask: Value,
        /// False for lanes 0 to 31, true for lanes 32 to 63.
        high: bool,
    },
    /// The lane's own bit of a 64-bit number: bit n of `high:low` for
    /// console lane n. Needs the console's lane number, like
    /// [`Op::BallotWord`]. One flag result.
    LaneBit {
        /// Bits 0 to 31.
        low: Value,
        /// Bits 32 to 63.
        high: Value,
    },
    /// Cross-lane, within the lane's quad of four: the lane in place p of
    /// its quad reads `value` of the lane in place `lanes[p]`. In a pixel
    /// program a quad is a 2 by 2 block of pixels: place 0 top left, 1 top
    /// right, 2 bottom left, 3 bottom right. A source lane that is not
    /// `active` gives zero. One word result.
    QuadPermute {
        /// The word read.
        value: Value,
        /// The source place for each of the four places.
        lanes: [u8; 4],
        /// Whether a lane may be read from.
        active: Value,
    },
    /// Cross-lane: whether any lane of the lane's quad holds the flag. One
    /// flag result.
    QuadAny(Value),
}

impl Op {
    /// The type of the operation's results, or `None` if it has none.
    pub fn result_type(&self) -> Option<Ty> {
        Some(match self {
            Op::Get(var) => var.ty(),
            Op::Set(..)
            | Op::StoreMemory { .. }
            | Op::StoreBuffer { .. }
            | Op::StoreFormat { .. }
            | Op::Export { .. }
            | Op::Discard(_) => return None,
            Op::Image(image) => match image.kind {
                ImageKind::Store { .. } => return None,
                _ => Ty::Word,
            },
            Op::Compare(..)
            | Op::FloatClass { .. }
            | Op::FlagNot(_)
            | Op::Flag(..)
            | Op::Carry { .. }
            | Op::Borrow { .. }
            | Op::AnyLane(_)
            | Op::LaneBit { .. }
            | Op::QuadAny(_) => Ty::Flag,
            // The type of a select is the type of what it selects, which
            // only the function knows; see `Function::value_type`.
            Op::Select { .. } => return None,
            _ => Ty::Word,
        })
    }

    /// How many temporaries the operation defines.
    pub fn result_count(&self) -> u8 {
        match self {
            Op::Set(..)
            | Op::StoreMemory { .. }
            | Op::StoreBuffer { .. }
            | Op::StoreFormat { .. }
            | Op::Export { .. }
            | Op::Discard(_) => 0,
            Op::LoadFormat { .. } => 4,
            Op::LoadMemory { words, .. }
            | Op::LoadBuffer { words, .. }
            | Op::LoadCode { words, .. } => *words,
            Op::Image(image) => match image.kind {
                ImageKind::Store { .. } => 0,
                _ => 4,
            },
            _ => 1,
        }
    }

    /// Whether the operation looks at other lanes of the wave.
    pub fn is_cross_lane(&self) -> bool {
        matches!(
            self,
            Op::FirstLane { .. }
                | Op::ReadLane { .. }
                | Op::CountLanes(_)
                | Op::FirstLaneIndex(_)
                | Op::AnyLane(_)
                | Op::LanesBelow { .. }
                | Op::BallotWord { .. }
                | Op::QuadPermute { .. }
                | Op::QuadAny(_)
        )
    }

    /// Calls `visit` on every value the operation reads.
    pub fn for_each_operand(&self, mut visit: impl FnMut(Value)) {
        match self {
            Op::Get(_) | Op::Input(_) => {}
            Op::Set(_, a)
            | Op::Unary(_, a)
            | Op::FlagNot(a)
            | Op::Discard(a)
            | Op::CountLanes(a)
            | Op::FirstLaneIndex(a)
            | Op::AnyLane(a)
            | Op::QuadAny(a) => visit(*a),
            Op::Binary(_, a, b) | Op::Compare(_, a, b) | Op::Flag(_, a, b) => {
                visit(*a);
                visit(*b);
            }
            Op::Ternary(_, a, b, c) => {
                visit(*a);
                visit(*b);
                visit(*c);
            }
            Op::FloatClass { value, classes } => {
                visit(*value);
                visit(*classes);
            }
            Op::Select {
                condition,
                if_true,
                if_false,
            } => {
                visit(*condition);
                visit(*if_true);
                visit(*if_false);
            }
            Op::Carry { a, b, carry_in } => {
                visit(*a);
                visit(*b);
                visit(*carry_in);
            }
            Op::Borrow { a, b, borrow_in } => {
                visit(*a);
                visit(*b);
                visit(*borrow_in);
            }
            Op::LoadMemory { address, .. } => {
                visit(address.low);
                visit(address.high);
            }
            Op::StoreMemory {
                address,
                values,
                guard,
                ..
            } => {
                visit(address.low);
                visit(address.high);
                values.iter().copied().for_each(&mut visit);
                visit(*guard);
            }
            Op::LoadBuffer {
                descriptor,
                position,
                ..
            } => {
                descriptor.iter().copied().for_each(&mut visit);
                visit(position.index);
                visit(position.offset);
            }
            Op::StoreBuffer {
                descriptor,
                position,
                values,
                guard,
                ..
            } => {
                descriptor.iter().copied().for_each(&mut visit);
                visit(position.index);
                visit(position.offset);
                values.iter().copied().for_each(&mut visit);
                visit(*guard);
            }
            Op::LoadFormat {
                descriptor,
                position,
                format,
            } => {
                descriptor.iter().copied().for_each(&mut visit);
                visit(position.index);
                visit(position.offset);
                visit(*format);
            }
            Op::StoreFormat {
                descriptor,
                position,
                format,
                values,
                guard,
            } => {
                descriptor.iter().copied().for_each(&mut visit);
                visit(position.index);
                visit(position.offset);
                visit(*format);
                values.iter().copied().for_each(&mut visit);
                visit(*guard);
            }
            Op::LoadCode { offset, .. } => visit(*offset),
            Op::Image(image) => {
                visit(image.texture);
                visit(image.format);
                if let Some(Sampler::Heap(index)) = image.sampler {
                    visit(index);
                }
                for value in [
                    image.offset,
                    image.bias,
                    image.reference,
                    image.level_clamp,
                    image.mip,
                ]
                .into_iter()
                .flatten()
                {
                    visit(value);
                }
                image.coordinates.iter().copied().for_each(&mut visit);
                match &image.level {
                    Level::Implicit | Level::Zero => {}
                    Level::Explicit(level) => visit(*level),
                    Level::Gradients(dx, dy) => {
                        dx.iter().chain(dy).copied().for_each(&mut visit);
                    }
                }
                if let ImageKind::Store { values, guard } = &image.kind {
                    values.iter().copied().for_each(&mut visit);
                    visit(*guard);
                }
            }
            Op::Export { values, guard, .. } => {
                values.iter().flatten().copied().for_each(&mut visit);
                visit(*guard);
            }
            Op::FirstLane { value, active } => {
                visit(*value);
                visit(*active);
            }
            Op::ReadLane { value, lane } => {
                visit(*value);
                visit(*lane);
            }
            Op::LanesBelow { mask, .. } | Op::BallotWord { mask, .. } => visit(*mask),
            Op::LaneBit { low, high } => {
                visit(*low);
                visit(*high);
            }
            Op::QuadPermute { value, active, .. } => {
                visit(*value);
                visit(*active);
            }
        }
    }
}

/// One instruction: an operation and the temporaries it defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inst {
    /// The first temporary the instruction defines. With several results
    /// they are numbered consecutively from it. Unused when the operation
    /// has no result.
    pub result: Temp,
    /// The operation.
    pub op: Op,
}

impl Inst {
    /// The temporaries the instruction defines.
    pub fn results(&self) -> impl Iterator<Item = Temp> + use<> {
        let first = self.result.0;
        (0..u32::from(self.op.result_count())).map(move |index| Temp(first + index))
    }
}

/// How a block ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// Goes on to another block.
    Jump(BlockId),
    /// Goes to `if_true` when the flag `condition` holds, else to
    /// `if_false`.
    Branch {
        /// The flag.
        condition: Value,
        /// The block when it holds.
        if_true: BlockId,
        /// The block when it does not.
        if_false: BlockId,
    },
    /// The program ends for this lane.
    Return,
}

impl Terminator {
    /// The blocks control can go to.
    pub fn successors(&self) -> impl Iterator<Item = BlockId> + use<> {
        let (first, second) = match *self {
            Terminator::Jump(to) => (Some(to), None),
            Terminator::Branch {
                if_true, if_false, ..
            } => (Some(if_true), Some(if_false)),
            Terminator::Return => (None, None),
        };
        first.into_iter().chain(second)
    }

    /// Replaces every successor `from` by `to`.
    pub fn retarget(&mut self, from: BlockId, to: BlockId) {
        match self {
            Terminator::Jump(target) => {
                if *target == from {
                    *target = to;
                }
            }
            Terminator::Branch {
                if_true, if_false, ..
            } => {
                if *if_true == from {
                    *if_true = to;
                }
                if *if_false == from {
                    *if_false = to;
                }
            }
            Terminator::Return => {}
        }
    }
}

/// A basic block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The byte offset in the machine code the block was lifted from, for
    /// messages. The entry block, which sets up the inputs, has none.
    pub offset: Option<u32>,
    /// The instructions, in order.
    pub insts: Vec<Inst>,
    /// How the block ends.
    pub terminator: Terminator,
}

/// One pixel attribute: which signature row an attribute number is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeRow {
    /// The attribute number interpolation names.
    pub attribute: u8,
    /// The register (row) of `ISG1`.
    pub row: u8,
    /// The components of the row the attribute carries.
    pub mask: u8,
    /// Whether the attribute is constant across the primitive.
    pub flat: bool,
}

/// One parameter export: which signature row a parameter number is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterRow {
    /// The number `ExportTarget::Parameter` carries.
    pub parameter: u8,
    /// The register (row) of `OSG1`.
    pub row: u8,
}

/// What a function takes from and gives to its surroundings, for an emitter
/// to declare.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Interface {
    /// Pixel: the attributes by number.
    pub pixel_inputs: Vec<AttributeRow>,
    /// Vertex: the parameter exports by number.
    pub outputs: Vec<ParameterRow>,
    /// Compute: the thread group's size.
    pub group_size: [u32; 3],
    /// Pixel: `DB_SHADER_CONTROL` as stored. Bit 0 says depth is exported.
    pub depth_control: u32,
}

/// A lifted program: what one lane does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// The hardware stage the program runs as.
    pub stage: HardwareStage,
    /// How many lanes the console runs the program on at once. Lane numbers
    /// in cross-lane operations count within such a wave.
    pub wave: WaveSize,
    /// The blocks. Block 0 is the entry.
    pub blocks: Vec<Block>,
    /// How many temporaries the function has; their numbers are below this.
    pub temps: u32,
    /// What the function takes in and puts out.
    pub interface: Interface,
    /// The program's own code words, which [`Op::LoadCode`] reads.
    pub code: Vec<u32>,
}

/// Something wrong with a function's shape, found by [`Function::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid intermediate form: {}", self.0)
    }
}

impl std::error::Error for Invalid {}

impl Function {
    /// The variables the function reads or writes, sorted.
    pub fn variables(&self) -> Vec<Var> {
        let mut vars = std::collections::BTreeSet::new();
        for block in &self.blocks {
            for inst in &block.insts {
                if let Op::Get(var) | Op::Set(var, _) = inst.op {
                    vars.insert(var);
                }
            }
        }
        vars.into_iter().collect()
    }

    /// The type of every temporary, by number. A temporary no instruction
    /// defines has none.
    pub fn temp_types(&self) -> Vec<Option<Ty>> {
        let mut types = vec![None; self.temps as usize];
        let value_type = |types: &[Option<Ty>], value: Value| match value {
            Value::Word(_) => Some(Ty::Word),
            Value::Flag(_) => Some(Ty::Flag),
            Value::Temp(temp) => types.get(temp.0 as usize).copied().flatten(),
        };
        for block in &self.blocks {
            for inst in &block.insts {
                let ty = match &inst.op {
                    Op::Select { if_true, .. } => value_type(&types, *if_true),
                    op => op.result_type(),
                };
                for temp in inst.results() {
                    if let Some(slot) = types.get_mut(temp.0 as usize) {
                        *slot = ty;
                    }
                }
            }
        }
        types
    }

    /// Whether any operation of the function looks across lanes.
    pub fn has_cross_lane(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| block.insts.iter().any(|inst| inst.op.is_cross_lane()))
    }

    /// Checks the rules of the form: every temporary is defined once, before
    /// its uses and in the same block; operand types fit; branch targets
    /// exist. The lift's output always passes; the check is for tests and
    /// for code that rewrites functions.
    pub fn verify(&self) -> Result<(), Invalid> {
        let mut defined = vec![false; self.temps as usize];
        let types = self.temp_types();
        for (id, block) in self.blocks.iter().enumerate() {
            let mut local = std::collections::HashSet::new();
            let type_of =
                |local: &std::collections::HashSet<Temp>, value: Value| -> Result<Ty, Invalid> {
                    match value {
                        Value::Word(_) => Ok(Ty::Word),
                        Value::Flag(_) => Ok(Ty::Flag),
                        Value::Temp(temp) => {
                            if !local.contains(&temp) {
                                return Err(Invalid(format!(
                                    "block {id} uses t{} before it is defined there",
                                    temp.0
                                )));
                            }
                            types[temp.0 as usize]
                                .ok_or_else(|| Invalid(format!("t{} has no type", temp.0)))
                        }
                    }
                };
            for inst in &block.insts {
                let mut problem = None;
                let mut operands = Vec::new();
                inst.op.for_each_operand(|value| operands.push(value));
                let mut operand_types = Vec::new();
                for value in operands {
                    match type_of(&local, value) {
                        Ok(ty) => operand_types.push(ty),
                        Err(error) => problem = Some(error),
                    }
                }
                if let Some(problem) = problem {
                    return Err(problem);
                }
                if let Some(expected) = expected_operand_types(&inst.op, &operand_types)
                    && expected != operand_types
                {
                    return Err(Invalid(format!(
                        "block {id}: operands of {:?} have the wrong types",
                        inst.op
                    )));
                }
                for temp in inst.results() {
                    let slot = defined
                        .get_mut(temp.0 as usize)
                        .ok_or_else(|| Invalid(format!("t{} is out of range", temp.0)))?;
                    if *slot {
                        return Err(Invalid(format!("t{} is defined twice", temp.0)));
                    }
                    *slot = true;
                    local.insert(temp);
                }
            }
            if let Terminator::Branch { condition, .. } = block.terminator
                && type_of(&local, condition)? != Ty::Flag
            {
                return Err(Invalid(format!("block {id} branches on a word")));
            }
            for successor in block.terminator.successors() {
                if successor >= self.blocks.len() {
                    return Err(Invalid(format!("block {id} goes to a missing block")));
                }
            }
        }
        Ok(())
    }
}

/// The operand types an operation requires, in the order
/// [`Op::for_each_operand`] visits them. `actual` is consulted where the
/// operation accepts either type.
fn expected_operand_types(op: &Op, actual: &[Ty]) -> Option<Vec<Ty>> {
    use Ty::{Flag, Word};
    Some(match op {
        Op::Get(_) | Op::Input(_) => vec![],
        Op::Set(var, _) => vec![var.ty()],
        Op::Unary(..) => vec![Word],
        Op::Binary(..) | Op::Compare(..) | Op::FloatClass { .. } => vec![Word, Word],
        Op::Ternary(..) => vec![Word; 3],
        Op::Select { .. } => vec![Flag, *actual.get(1)?, *actual.get(1)?],
        Op::FlagNot(_)
        | Op::Discard(_)
        | Op::CountLanes(_)
        | Op::FirstLaneIndex(_)
        | Op::AnyLane(_)
        | Op::QuadAny(_)
        | Op::LanesBelow { .. }
        | Op::BallotWord { .. } => vec![Flag],
        Op::Flag(..) => vec![Flag, Flag],
        Op::Carry { .. } | Op::Borrow { .. } => vec![Word, Word, Flag],
        Op::LoadMemory { .. } | Op::LaneBit { .. } | Op::ReadLane { .. } => vec![Word, Word],
        Op::StoreMemory { values, .. } => {
            let mut types = vec![Word; 2 + values.len()];
            types.push(Flag);
            types
        }
        Op::LoadBuffer { .. } => vec![Word; 6],
        Op::StoreBuffer { values, .. } => {
            let mut types = vec![Word; 6 + values.len()];
            types.push(Flag);
            types
        }
        Op::LoadFormat { .. } => vec![Word; 7],
        Op::StoreFormat { .. } => {
            let mut types = vec![Word; 11];
            types.push(Flag);
            types
        }
        Op::LoadCode { .. } => vec![Word],
        Op::Image(image) => {
            let mut types = vec![Word; actual.len()];
            if matches!(image.kind, ImageKind::Store { .. }) {
                *types.last_mut()? = Flag;
            }
            types
        }
        Op::Export { .. } => {
            let mut types = vec![Word; actual.len()];
            *types.last_mut()? = Flag;
            types
        }
        Op::FirstLane { .. } | Op::QuadPermute { .. } => vec![Word, Flag],
    })
}
