//! Writes the HLSL of one function of the intermediate form.
//!
//! The shader that comes out has this order: the stage's inputs block, the
//! helper functions the program uses ([`super::helpers`]), the values the
//! stage hands in and takes out as `static` variables, the image functions
//! ([`super::image`]), `void program()` with the function's code, and
//! `main`, which fills the statics from its parameters, runs the program
//! and returns what it exported.
//!
//! In `program`, every variable of the intermediate form is a local that
//! starts as zero (the live flag as true), every temporary is a local of
//! the block it is defined in, and control flow is the structured code of
//! [`super::structure`]:
//!
//! - a scope that jumps leave is `do { … } while (false);` and the jump is
//!   `break;`
//! - a loop is `for (; fuel != 0u; fuel--) { … }` and a back edge is
//!   `continue;`
//! - a jump that leaves more than one of these at once sets the variable
//!   `go` to the number of the scope it is headed for and breaks; after
//!   each scope on the way out a line passes it on, and the scope it is
//!   headed for clears it.
//!
//! `fuel` is one count for all loops of an invocation: unmirrored data can
//! make a console loop run four billion times, which would hang the host's
//! GPU (section 8 of the research note). A lane that runs out ends where it
//! is.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::container::signature::{ComponentType, Signature};
use crate::container::xbox::HardwareStage;
use crate::ir::cfg::ControlFlow;
use crate::ir::eval::{BufferFormat, ChannelKind};
use crate::ir::{
    BinaryOp, BlockId, CompareOp, ExportTarget, FlagOp, Function, ImageKind, Input, Inst, Op,
    Sampler, Terminator, TernaryOp, Ty, UnaryOp, Value, Var, VertexTerm, Weights,
};

use super::helpers::HELPERS;
use super::image::{self, Argument};
use super::structure::{Node, structure};
use super::{EmitError, Options, TargetKind};

/// The signatures of the container a function came from, for the names and
/// types a vertex shader's inputs are declared with and for the clip and
/// cull distances it puts out.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Signatures<'a> {
    /// `ISG1`.
    pub(crate) inputs: Option<&'a Signature>,
    /// `OSG1`.
    pub(crate) outputs: Option<&'a Signature>,
}

/// The HLSL of one function, and what the host has to know about it. The
/// fields are those of the same names in [`super::Shader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emitted {
    /// The HLSL.
    pub source: String,
    /// The samplers the root signature fixes, in the order of the stage
    /// input words that carry their places.
    pub samplers: Vec<[u32; 4]>,
    /// How many rows the shader declares.
    pub rows: u32,
    /// Which rows carry bits that must not be interpolated.
    pub flat_rows: u32,
    /// Whether the control flow was written as a loop around a `switch`,
    /// because the graph could not be structured.
    pub dispatched: bool,
}

/// How many trips all loops of one invocation may make together.
fn fuel(stage: HardwareStage) -> u32 {
    match stage {
        HardwareStage::Compute => 65_536,
        _ => 4_096,
    }
}

/// The most samplers a root signature may fix: the stage input words 67 to
/// 77 carry their places.
const FIXED_SAMPLERS: usize = 11;

const CHANNELS: [&str; 4] = ["x", "y", "z", "w"];

/// The six pairs of weights, in the order the names below are in.
const WEIGHTS: [Weights; 6] = [
    Weights::PerspectiveSample,
    Weights::PerspectiveCenter,
    Weights::PerspectiveCentroid,
    Weights::LinearSample,
    Weights::LinearCenter,
    Weights::LinearCentroid,
];

/// The name each pair of weights has in the shader.
const WEIGHT_NAMES: [&str; 6] = [
    "sample",
    "center",
    "centroid",
    "flat_sample",
    "flat_center",
    "flat_centroid",
];

/// The interpolation modifier that makes the host interpolate a row with a
/// pair of weights.
const WEIGHT_MODIFIERS: [&str; 6] = [
    "sample ",
    "",
    "centroid ",
    "noperspective sample ",
    "noperspective ",
    "noperspective centroid ",
];

fn weights_index(weights: Weights) -> usize {
    WEIGHTS
        .iter()
        .position(|&candidate| candidate == weights)
        .unwrap_or(1)
}

/// Whether a pair of weights is linear in screen space.
fn is_linear(index: usize) -> bool {
    index >= 3
}

/// A word as an HLSL literal.
fn word(value: u32) -> String {
    if value < 16 {
        format!("{value}u")
    } else {
        format!("{value:#x}u")
    }
}

/// How one pixel attribute row is read.
#[derive(Debug, Default, Clone)]
struct RowUse {
    /// The pairs of weights it is interpolated with, by index.
    interpolated: BTreeSet<usize>,
    /// Which of the per-vertex terms P0, P10, P20 are read.
    terms: [bool; 3],
}

impl RowUse {
    fn any_term(&self) -> bool {
        self.terms.iter().any(|&term| term)
    }

    /// Whether the shader works the row out itself from the three vertices'
    /// values: when it is read both ways, or with weights of both kinds.
    fn by_hand(&self) -> bool {
        let both_kinds = self.interpolated.iter().any(|&index| is_linear(index))
            && self.interpolated.iter().any(|&index| !is_linear(index));
        (self.any_term() && !self.interpolated.is_empty()) || both_kinds
    }
}

/// What a function takes from and gives to its stage, gathered before any
/// code is written.
#[derive(Debug, Default)]
struct Uses {
    vertex_id: bool,
    instance_id: bool,
    /// The signature elements a vertex program reads, by index.
    elements: BTreeSet<usize>,
    rows: BTreeMap<u8, RowUse>,
    /// The pairs of weights the program needs as numbers.
    weights: BTreeSet<usize>,
    position: bool,
    front: bool,
    sample: bool,
    coverage: bool,
    quad: bool,
    thread: bool,
    group: bool,
    colors: BTreeSet<u8>,
    depth: [bool; 3],
    parameters: BTreeSet<u8>,
}

/// One scope of the structured code being written.
struct Scope {
    /// The loop's header, or the block that follows the scope.
    block: BlockId,
    is_loop: bool,
    /// The number `go` holds for a jump headed here.
    id: u32,
    /// Whether a jump from deeper inside is headed here.
    lands: bool,
    /// The scopes (by depth) that jumps passing out of this one head for.
    passes: BTreeSet<usize>,
}

struct Emitter<'a> {
    function: &'a Function,
    options: &'a Options,
    signatures: Signatures<'a>,
    types: Vec<Option<Ty>>,
    helpers: BTreeSet<&'static str>,
    /// The distinct image functions, in order of first use.
    images: Vec<String>,
    samplers: Vec<[u32; 4]>,
    uses: Uses,
    /// Whether the program reads its own code through a computed offset.
    code_table: bool,
    /// The temporaries known to be multiples of four.
    aligned: BTreeSet<u32>,
    /// The temporaries of the block being written that stand for a variable
    /// they were read from, and are written as its name.
    names: BTreeMap<u32, String>,
    scopes: Vec<Scope>,
    next_scope: u32,
    uses_go: bool,
    uses_fuel: bool,
    /// How many clip and cull distances the output signature declares.
    distances: [u32; 2],
}

/// Writes the HLSL of `function`.
pub(crate) fn emit(
    function: &Function,
    signatures: Signatures<'_>,
    options: &Options,
) -> Result<Emitted, EmitError> {
    if !matches!(
        function.stage,
        HardwareStage::Vertex | HardwareStage::Pixel | HardwareStage::Compute
    ) {
        return Err(EmitError {
            op: "the function",
            reason: "only vertex, pixel and compute programs have a host shader",
        });
    }
    let mut distances = [0u32; 2];
    if let Some(outputs) = signatures.outputs {
        for element in &outputs.elements {
            let count = u32::from(element.mask).count_ones();
            if element
                .semantic_name
                .eq_ignore_ascii_case("SV_ClipDistance")
            {
                distances[0] += count;
            } else if element
                .semantic_name
                .eq_ignore_ascii_case("SV_CullDistance")
            {
                distances[1] += count;
            }
        }
    }
    let mut emitter = Emitter {
        function,
        options,
        signatures,
        types: function.temp_types(),
        helpers: BTreeSet::new(),
        images: Vec::new(),
        samplers: Vec::new(),
        uses: Uses::default(),
        code_table: false,
        aligned: BTreeSet::new(),
        names: BTreeMap::new(),
        scopes: Vec::new(),
        next_scope: 1,
        uses_go: false,
        uses_fuel: false,
        distances,
    };
    emitter.gather()?;
    emitter.finish()
}

impl Emitter<'_> {
    fn stage(&self) -> HardwareStage {
        self.function.stage
    }

    fn unsupported<T>(&self, op: &'static str, reason: &'static str) -> Result<T, EmitError> {
        Err(EmitError { op, reason })
    }

    /// Notes that the shader calls a helper.
    fn need(&mut self, name: &'static str) {
        self.helpers.insert(name);
    }

    /// The text of a value.
    fn v(&self, value: Value) -> String {
        match value {
            Value::Temp(temp) => match self.names.get(&temp.0) {
                Some(name) => name.clone(),
                None => format!("t{}", temp.0),
            },
            Value::Word(value) => word(value),
            Value::Flag(flag) => flag.to_string(),
        }
    }

    fn var(var: Var) -> String {
        match var {
            Var::Vector(n) => format!("v{n}"),
            Var::Scalar(n) => format!("s{n}"),
            Var::Mask(n) => format!("m{n}"),
            Var::Exec => "exec".to_string(),
            Var::Vcc => "vcc".to_string(),
            Var::Scc => "scc".to_string(),
            Var::M0 => "reg_m0".to_string(),
            Var::Exported => "exported".to_string(),
        }
    }

    /// The four bits of a constant lane mask that repeats every four lanes
    /// over the whole wave, or `None` if it does not.
    fn quad_pattern(&self, low: u32, high: u32) -> Option<u32> {
        let pattern = low & 0xF;
        let repeated = pattern * 0x1111_1111;
        let whole = match self.function.wave {
            crate::gfx10::WaveSize::Wave32 => low == repeated,
            crate::gfx10::WaveSize::Wave64 => low == repeated && high == repeated,
        };
        whole.then_some(pattern)
    }

    /// The signature element and component a vertex attribute channel is
    /// read from, if the signature has one for it.
    fn element_of(&self, row: u8, channel: u8) -> Option<(usize, u8)> {
        let inputs = self.signatures.inputs?;
        let in_row = |element: &&crate::container::signature::SignatureElement| {
            element.register == u32::from(row) && element.mask != 0
        };
        let start = |mask: u8| mask.trailing_zeros() as u8;
        // The element that holds the channel; or, for a channel past what
        // the signature declares, the element before it, whose missing
        // components the input assembler fills in.
        let (index, element) = inputs
            .elements
            .iter()
            .enumerate()
            .filter(|(_, element)| in_row(element))
            .filter(|(_, element)| start(element.mask) <= channel)
            .max_by_key(|(_, element)| start(element.mask))?;
        Some((index, channel - start(element.mask)))
    }

    /// Looks through the function for what it takes in and puts out.
    fn gather(&mut self) -> Result<(), EmitError> {
        let function = self.function;
        for block in &function.blocks {
            for inst in &block.insts {
                match &inst.op {
                    Op::Input(input) => self.gather_input(*input)?,
                    Op::Export { target, values, .. } => match *target {
                        ExportTarget::Color(n) => {
                            self.uses.colors.insert(n);
                        }
                        ExportTarget::Depth => {
                            for (channel, value) in values.iter().take(3).enumerate() {
                                self.uses.depth[channel] |= value.is_some();
                            }
                        }
                        ExportTarget::Parameter(n) => {
                            self.uses.parameters.insert(n);
                        }
                        ExportTarget::Position(_) => {}
                    },
                    Op::QuadPermute { .. } | Op::QuadAny(_) => {
                        self.uses.quad = true;
                        self.uses.position = true;
                    }
                    Op::LaneBit { low, high } => {
                        if let (Some(low), Some(high)) = (low.as_word(), high.as_word())
                            && self.quad_pattern(low, high).is_some()
                        {
                            self.uses.quad = true;
                            self.uses.position = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        // A row that is worked out by hand needs its weights as numbers.
        let by_hand: Vec<usize> = self
            .uses
            .rows
            .values()
            .filter(|row| row.by_hand())
            .flat_map(|row| row.interpolated.iter().copied())
            .collect();
        self.uses.weights.extend(by_hand);
        Ok(())
    }

    fn gather_input(&mut self, input: Input) -> Result<(), EmitError> {
        let stage = self.stage();
        let wrong = EmitError {
            op: "Input",
            reason: "an input of another stage",
        };
        let need = |wanted: HardwareStage| if stage == wanted { Ok(()) } else { Err(wrong) };
        match input {
            Input::UserData(n) => {
                if n >= 64 {
                    return self.unsupported("Input", "a user-data word past the sixty-fourth");
                }
            }
            Input::VertexId => {
                need(HardwareStage::Vertex)?;
                self.uses.vertex_id = true;
            }
            Input::InstanceId => {
                need(HardwareStage::Vertex)?;
                self.uses.instance_id = true;
            }
            Input::VertexAttribute { row, channel } => {
                need(HardwareStage::Vertex)?;
                if channel > 3 {
                    return self.unsupported("Input", "a vertex attribute channel past the fourth");
                }
                if let Some((element, _)) = self.element_of(row, channel) {
                    self.uses.elements.insert(element);
                }
            }
            Input::Weight { weights, .. } => {
                need(HardwareStage::Pixel)?;
                self.uses.weights.insert(weights_index(weights));
            }
            Input::Attribute {
                attribute,
                channel,
                term,
            } => {
                need(HardwareStage::Pixel)?;
                if attribute >= 32 || channel > 3 {
                    return self.unsupported("Input", "an attribute past the thirty-second");
                }
                let row = self.uses.rows.entry(attribute).or_default();
                row.terms[match term {
                    VertexTerm::P0 => 0,
                    VertexTerm::P10 => 1,
                    VertexTerm::P20 => 2,
                }] = true;
            }
            Input::Interpolated {
                attribute,
                channel,
                weights,
            } => {
                need(HardwareStage::Pixel)?;
                if attribute >= 32 || channel > 3 {
                    return self.unsupported("Input", "an attribute past the thirty-second");
                }
                let row = self.uses.rows.entry(attribute).or_default();
                row.interpolated.insert(weights_index(weights));
            }
            Input::Position(component) => {
                need(HardwareStage::Pixel)?;
                if component > 3 {
                    return self.unsupported("Input", "a position component past the fourth");
                }
                self.uses.position = true;
            }
            Input::FrontFace => {
                need(HardwareStage::Pixel)?;
                self.uses.front = true;
            }
            Input::Ancillary => {
                need(HardwareStage::Pixel)?;
                self.uses.sample = true;
            }
            Input::SampleCoverage => {
                need(HardwareStage::Pixel)?;
                self.uses.coverage = true;
            }
            Input::FixedPosition => {
                need(HardwareStage::Pixel)?;
                self.uses.position = true;
            }
            Input::ThreadId(axis) | Input::GroupId(axis) => {
                need(HardwareStage::Compute)?;
                if axis > 2 {
                    return self.unsupported("Input", "a thread or group axis past the third");
                }
                if matches!(input, Input::ThreadId(_)) {
                    self.uses.thread = true;
                } else {
                    self.uses.group = true;
                }
            }
        }
        Ok(())
    }

    /// The text that reads a stage input as a word.
    fn input(&mut self, input: Input) -> String {
        match input {
            Input::UserData(n) => {
                format!("input_words[{}].{}", n >> 2, CHANNELS[usize::from(n & 3)])
            }
            Input::VertexId => "in_vertex".to_string(),
            Input::InstanceId => "in_instance".to_string(),
            Input::VertexAttribute { row, channel } => match self.element_of(row, channel) {
                Some((element, component)) => {
                    let name = format!("in_a{element}.{}", CHANNELS[usize::from(component & 3)]);
                    match self.element_type(element) {
                        "float4" => format!("asuint({name})"),
                        "int4" => format!("(uint){name}"),
                        _ => name,
                    }
                }
                // What the input assembler gives for a channel nothing
                // feeds: 0, 0, 0, 1.
                None if channel == 3 => word(1f32.to_bits()),
                None => word(0),
            },
            Input::Weight { weights, second } => format!(
                "asuint(in_weights_{}.{})",
                WEIGHT_NAMES[weights_index(weights)],
                if second { "z" } else { "y" }
            ),
            Input::Attribute {
                attribute,
                channel,
                term,
            } => {
                let at = |vertex: u32| {
                    format!(
                        "asuint(in_row{attribute}_v{vertex}.{})",
                        CHANNELS[usize::from(channel & 3)]
                    )
                };
                match term {
                    VertexTerm::P0 => at(0),
                    VertexTerm::P10 => {
                        self.need("f_sub");
                        format!("f_sub({}, {})", at(1), at(0))
                    }
                    VertexTerm::P20 => {
                        self.need("f_sub");
                        format!("f_sub({}, {})", at(2), at(0))
                    }
                }
            }
            Input::Interpolated {
                attribute,
                channel,
                weights,
            } => format!(
                "asuint(in_row{attribute}_{}.{})",
                WEIGHT_NAMES[weights_index(weights)],
                CHANNELS[usize::from(channel & 3)]
            ),
            Input::Position(3) => {
                // The console hands over one over w; the host hands over w.
                self.need("f_rcp");
                "f_rcp(asuint(in_position.w))".to_string()
            }
            Input::Position(component) => {
                format!(
                    "asuint(in_position.{})",
                    CHANNELS[usize::from(component & 3)]
                )
            }
            Input::FrontFace => "select(in_front, 0xffffffffu, 0u)".to_string(),
            Input::Ancillary => "(in_sample << 8)".to_string(),
            Input::SampleCoverage => "in_coverage".to_string(),
            Input::FixedPosition => {
                "((uint)in_position.x | ((uint)in_position.y << 16))".to_string()
            }
            Input::ThreadId(axis) => format!("in_thread.{}", CHANNELS[usize::from(axis & 3)]),
            Input::GroupId(axis) => format!("in_group.{}", CHANNELS[usize::from(axis & 3)]),
        }
    }

    /// The HLSL type a signature element is declared with: always four
    /// wide, so that the input assembler fills in what the title's vertex
    /// layout leaves out.
    fn element_type(&self, element: usize) -> &'static str {
        let ty = self
            .signatures
            .inputs
            .and_then(|inputs| inputs.elements.get(element))
            .map(|element| element.component_type);
        match ty {
            Some(ComponentType::UInt32 | ComponentType::UInt16 | ComponentType::UInt64) => "uint4",
            Some(ComponentType::SInt32 | ComponentType::SInt16 | ComponentType::SInt64) => "int4",
            _ => "float4",
        }
    }

    /// A float comparison that is false when an operand is a NaN.
    fn ordered(&mut self, a: &str, b: &str, operator: &str) -> String {
        self.need("f_ord");
        format!("and(f_ord({a}, {b}), asfloat({a}) {operator} asfloat({b}))")
    }

    fn unary(&mut self, op: UnaryOp, a: &str) -> String {
        let call = |emitter: &mut Self, name: &'static str| {
            emitter.need(name);
            format!("{name}({a})")
        };
        match op {
            UnaryOp::Not => format!("~{a}"),
            UnaryOp::CountBits => format!("countbits({a})"),
            UnaryOp::FindLowBit => format!("select({a} == 0u, 0xffffffffu, firstbitlow({a}))"),
            UnaryOp::LeadingZeros => {
                format!("select({a} == 0u, 0xffffffffu, 31u - firstbithigh({a}))")
            }
            UnaryOp::LeadingSignBits => format!(
                "select(or({a} == 0u, {a} == 0xffffffffu), 0xffffffffu, 31u - firstbithigh(max(select(({a} >> 31) != 0u, ~{a}, {a}), 1u)))"
            ),
            UnaryOp::ReverseBits => format!("reversebits({a})"),
            UnaryOp::Saturate => call(self, "f_saturate"),
            UnaryOp::Reciprocal => call(self, "f_rcp"),
            UnaryOp::ReciprocalSqrt => call(self, "f_rsqrt"),
            UnaryOp::Sqrt => call(self, "f_sqrt"),
            UnaryOp::Exp2 => call(self, "f_exp2"),
            UnaryOp::Log2 => call(self, "f_log2"),
            UnaryOp::SinTurns => {
                self.need("f_turns");
                format!("f_turns({a}, false)")
            }
            UnaryOp::CosTurns => {
                self.need("f_turns");
                format!("f_turns({a}, true)")
            }
            UnaryOp::Floor => format!("asuint(floor(asfloat({a})))"),
            UnaryOp::Ceil => format!("asuint(ceil(asfloat({a})))"),
            UnaryOp::Trunc => format!("asuint(trunc(asfloat({a})))"),
            UnaryOp::RoundEven => format!("asuint(round(asfloat({a})))"),
            UnaryOp::Fract => call(self, "f_fract"),
            UnaryOp::FloatFromSigned => format!("asuint((float)(int){a})"),
            UnaryOp::FloatFromUnsigned => call(self, "f_from_u"),
            UnaryOp::SignedFromFloat => call(self, "f_to_i"),
            UnaryOp::UnsignedFromFloat => call(self, "f_to_u"),
            UnaryOp::HalfFromFloat => {
                self.need("half_from_f");
                format!("half_from_f({a}, false)")
            }
            UnaryOp::HalfFromFloatTowardZero => {
                self.need("half_from_f");
                format!("half_from_f({a}, true)")
            }
            UnaryOp::FloatFromHalf => call(self, "f_from_half"),
        }
    }

    fn binary(&mut self, op: BinaryOp, a: &str, b: &str) -> String {
        let call = |emitter: &mut Self, name: &'static str| {
            emitter.need(name);
            format!("{name}({a}, {b})")
        };
        match op {
            BinaryOp::Add => format!("{a} + {b}"),
            BinaryOp::Sub => format!("{a} - {b}"),
            BinaryOp::Mul => format!("{a} * {b}"),
            BinaryOp::MulHighUnsigned => call(self, "mul_hi_u"),
            BinaryOp::MulHighSigned => call(self, "mul_hi_s"),
            BinaryOp::And => format!("{a} & {b}"),
            BinaryOp::Or => format!("{a} | {b}"),
            BinaryOp::Xor => format!("{a} ^ {b}"),
            BinaryOp::ShiftLeft => format!("{a} << ({b} & 31u)"),
            BinaryOp::ShiftRight => format!("{a} >> ({b} & 31u)"),
            BinaryOp::ShiftRightSigned => format!("(uint)((int){a} >> ({b} & 31u))"),
            BinaryOp::MinUnsigned => format!("min({a}, {b})"),
            BinaryOp::MaxUnsigned => format!("max({a}, {b})"),
            BinaryOp::MinSigned => format!("(uint)min((int){a}, (int){b})"),
            BinaryOp::MaxSigned => format!("(uint)max((int){a}, (int){b})"),
            BinaryOp::FloatAdd => call(self, "f_add"),
            BinaryOp::FloatSub => call(self, "f_sub"),
            BinaryOp::FloatMul => call(self, "f_mul"),
            BinaryOp::FloatMulLegacy => call(self, "f_mul_legacy"),
            BinaryOp::FloatMin => call(self, "f_min"),
            BinaryOp::FloatMax => call(self, "f_max"),
            BinaryOp::FloatScale => call(self, "f_scale"),
        }
    }

    fn ternary(&mut self, op: TernaryOp, a: &str, b: &str, c: &str) -> String {
        let (helper, name) = match op {
            TernaryOp::FloatFused => ("f_fused", "f_fused"),
            TernaryOp::FloatMulAdd => ("f_mul_add", "f_mul_add"),
            TernaryOp::FloatMedian => ("f_med3", "f_med3"),
            TernaryOp::PermuteBytes => ("perm_bytes", "perm_bytes"),
            TernaryOp::CubeFace => ("cube", "cube_face"),
            TernaryOp::CubeS => ("cube", "cube_s"),
            TernaryOp::CubeT => ("cube", "cube_t"),
            TernaryOp::CubeMajor => ("cube", "cube_major"),
        };
        self.need(helper);
        format!("{name}({a}, {b}, {c})")
    }

    fn compare(&mut self, op: CompareOp, a: &str, b: &str) -> String {
        let signed = |operator: &str| format!("(int){a} {operator} (int){b}");
        match op {
            CompareOp::Equal => format!("{a} == {b}"),
            CompareOp::NotEqual => format!("{a} != {b}"),
            CompareOp::UnsignedLess => format!("{a} < {b}"),
            CompareOp::UnsignedLessEqual => format!("{a} <= {b}"),
            CompareOp::UnsignedGreater => format!("{a} > {b}"),
            CompareOp::UnsignedGreaterEqual => format!("{a} >= {b}"),
            CompareOp::SignedLess => signed("<"),
            CompareOp::SignedLessEqual => signed("<="),
            CompareOp::SignedGreater => signed(">"),
            CompareOp::SignedGreaterEqual => signed(">="),
            CompareOp::FloatEqual => self.ordered(a, b, "=="),
            CompareOp::FloatLess => self.ordered(a, b, "<"),
            CompareOp::FloatLessEqual => self.ordered(a, b, "<="),
            CompareOp::FloatGreater => self.ordered(a, b, ">"),
            CompareOp::FloatGreaterEqual => self.ordered(a, b, ">="),
            CompareOp::FloatLessOrGreater => self.ordered(a, b, "!="),
            CompareOp::FloatOrdered => {
                self.need("f_ord");
                format!("f_ord({a}, {b})")
            }
            CompareOp::FloatUnordered => {
                self.need("f_ord");
                format!("!f_ord({a}, {b})")
            }
            CompareOp::FloatNotEqual => format!("!{}", self.ordered(a, b, "==")),
            CompareOp::FloatNotLess => format!("!{}", self.ordered(a, b, "<")),
            CompareOp::FloatNotLessEqual => format!("!{}", self.ordered(a, b, "<=")),
            CompareOp::FloatNotGreater => format!("!{}", self.ordered(a, b, ">")),
            CompareOp::FloatNotGreaterEqual => format!("!{}", self.ordered(a, b, ">=")),
            CompareOp::FloatNotLessOrGreater => format!("!{}", self.ordered(a, b, "!=")),
        }
    }

    /// The text of a buffer descriptor's four words as one vector.
    fn descriptor(&self, words: &[Value; 4]) -> String {
        format!(
            "uint4({}, {}, {}, {})",
            self.v(words[0]),
            self.v(words[1]),
            self.v(words[2]),
            self.v(words[3])
        )
    }

    /// The text of an offset `bytes` past a value.
    fn offset(&self, base: Value, bytes: u32) -> String {
        match base {
            Value::Word(base) => word(base.wrapping_add(bytes)),
            _ if bytes == 0 => self.v(base),
            _ => format!("({} + {})", self.v(base), word(bytes)),
        }
    }

    /// The place in the sampler heap of an operation's sampler, as text.
    fn sampler(&mut self, sampler: Sampler) -> Result<String, EmitError> {
        match sampler {
            Sampler::Heap(place) => Ok(self.v(place)),
            Sampler::Immediate(words) => {
                let index = match self.samplers.iter().position(|known| *known == words) {
                    Some(index) => index,
                    None => {
                        if self.samplers.len() == FIXED_SAMPLERS {
                            return self.unsupported(
                                "Image",
                                "more samplers fixed by the root signature than the stage inputs have places for",
                            );
                        }
                        self.samplers.push(words);
                        self.samplers.len() - 1
                    }
                };
                let n = 67 + index;
                Ok(format!("input_words[{}].{}", n >> 2, CHANNELS[n & 3]))
            }
        }
    }

    /// Writes one instruction.
    fn inst(&mut self, inst: &Inst, indent: &str, out: &mut String) -> Result<(), EmitError> {
        let first = inst.result.0;
        let pixel = self.stage() == HardwareStage::Pixel;
        let ty = |emitter: &Self| match emitter.types.get(first as usize).copied().flatten() {
            Some(Ty::Flag) => "bool",
            _ => "uint",
        };
        // Most operations define one temporary from one expression.
        let expression: String = match &inst.op {
            Op::Get(var) => Self::var(*var),
            Op::Set(var, value) => {
                let _ = writeln!(out, "{indent}{} = {};", Self::var(*var), self.v(*value));
                return Ok(());
            }
            Op::Unary(op, a) => {
                let a = self.v(*a);
                self.unary(*op, &a)
            }
            Op::Binary(
                op @ (BinaryOp::ShiftLeft | BinaryOp::ShiftRight | BinaryOp::ShiftRightSigned),
                a,
                Value::Word(by),
            ) => {
                // A shift by a constant: the mask to five bits is done here.
                let (a, by) = (self.v(*a), by & 31);
                match op {
                    BinaryOp::ShiftLeft => format!("{a} << {by}u"),
                    BinaryOp::ShiftRight => format!("{a} >> {by}u"),
                    _ => format!("(uint)((int){a} >> {by}u)"),
                }
            }
            Op::Binary(op, a, b) => {
                if let (BinaryOp::And, Value::Word(mask)) = (op, b)
                    && mask & 3 == 0
                {
                    self.aligned.insert(first);
                }
                let (a, b) = (self.v(*a), self.v(*b));
                self.binary(*op, &a, &b)
            }
            Op::Ternary(op, a, b, c) => {
                let (a, b, c) = (self.v(*a), self.v(*b), self.v(*c));
                self.ternary(*op, &a, &b, &c)
            }
            Op::Compare(op, a, b) => {
                let (a, b) = (self.v(*a), self.v(*b));
                self.compare(*op, &a, &b)
            }
            Op::FloatClass { value, classes } => {
                self.need("f_class");
                format!("f_class({}, {})", self.v(*value), self.v(*classes))
            }
            Op::Select {
                condition,
                if_true,
                if_false,
            } => format!(
                "select({}, {}, {})",
                self.v(*condition),
                self.v(*if_true),
                self.v(*if_false)
            ),
            Op::FlagNot(a) => format!("!{}", self.v(*a)),
            Op::Flag(op, a, b) => {
                let (a, b) = (self.v(*a), self.v(*b));
                match op {
                    FlagOp::And => format!("and({a}, {b})"),
                    FlagOp::Or => format!("or({a}, {b})"),
                    FlagOp::Xor => format!("{a} != {b}"),
                }
            }
            Op::Carry { a, b, carry_in } => {
                self.need("carry");
                format!(
                    "carry({}, {}, {})",
                    self.v(*a),
                    self.v(*b),
                    self.v(*carry_in)
                )
            }
            Op::Borrow { a, b, borrow_in } => {
                self.need("borrow");
                format!(
                    "borrow({}, {}, {})",
                    self.v(*a),
                    self.v(*b),
                    self.v(*borrow_in)
                )
            }
            Op::Input(input) => self.input(*input),
            Op::LoadMemory {
                address,
                width,
                words,
            } => {
                self.need("mem_load");
                let (low, high) = (self.v(address.low), self.v(address.high));
                // A word at an address the program has just rounded down to
                // a multiple of four lies in one word of memory.
                let aligned = *width == 32
                    && match address.low {
                        Value::Word(low) => low & 3 == 0,
                        Value::Temp(temp) => self.aligned.contains(&temp.0),
                        Value::Flag(_) => false,
                    };
                if *width == 32 && !aligned {
                    // Words in a row at an address that may not be a
                    // multiple of four: the words of memory that hold them,
                    // each read once, and one more only if it is not.
                    let count = u32::from(*words);
                    for unit in 0..count {
                        let _ = writeln!(
                            out,
                            "{indent}uint w{first}_{unit} = mem_load32_aligned_at({low}, {high}, {});",
                            word(unit * 4)
                        );
                    }
                    let _ = writeln!(
                        out,
                        "{indent}uint w{first}_{count} = 0u;
{indent}if (({low} & 3u) != 0u) w{first}_{count} = mem_load32_aligned_at({low}, {high}, {});",
                        word(count * 4)
                    );
                    for unit in 0..count {
                        let _ = writeln!(
                            out,
                            "{indent}uint t{} = mem_pick({low}, w{first}_{unit}, w{first}_{});",
                            first + unit,
                            unit + 1
                        );
                    }
                    return Ok(());
                }
                let bytes = u32::from(*width / 8);
                let width = if aligned {
                    "32_aligned".to_string()
                } else {
                    width.to_string()
                };
                for unit in 0..u32::from(*words) {
                    let offset = unit * bytes;
                    let call = if offset == 0 {
                        format!("mem_load{width}({low}, {high})")
                    } else {
                        format!("mem_load{width}_at({low}, {high}, {})", word(offset))
                    };
                    let _ = writeln!(out, "{indent}uint t{} = {call};", first + unit);
                }
                return Ok(());
            }
            Op::StoreMemory {
                address,
                width,
                values,
                guard,
            } => {
                self.need("mem_store");
                let (low, high) = (self.v(address.low), self.v(address.high));
                let mask = word(mask_of(*width));
                let _ = writeln!(out, "{indent}if ({}) {{", self.v(*guard));
                for (unit, value) in values.iter().enumerate() {
                    let offset = unit as u32 * u32::from(*width / 8);
                    let _ = writeln!(
                        out,
                        "{indent}    mem_store_at({low}, {high}, {}, {}, {mask});",
                        word(offset),
                        self.v(*value)
                    );
                }
                let _ = writeln!(out, "{indent}}}");
                return Ok(());
            }
            Op::LoadBuffer {
                descriptor,
                position,
                width,
                words,
            } => {
                self.need("buf_load");
                let descriptor = self.descriptor(descriptor);
                let index = self.v(position.index);
                let count = u32::from(*words);
                // Words that follow one another in memory: one word, or
                // several at a constant offset that cannot wrap. They are
                // read as the words of memory that hold them, each once,
                // and one more only if the address is not a multiple of
                // four, which a compiler's own loads never are.
                let in_a_row = *width == 32
                    && (count == 1
                        || position
                            .offset
                            .as_word()
                            .is_some_and(|offset| offset.checked_add(4 * count).is_some()));
                if in_a_row {
                    let offset = self.offset(position.offset, 0);
                    let _ = writeln!(
                        out,
                        "{indent}uint3 a{first} = buf_locate({descriptor}, {index}, {offset});"
                    );
                    for unit in 0..count {
                        let _ = writeln!(
                            out,
                            "{indent}uint w{first}_{unit} = mem_load32_aligned_at(a{first}.x, a{first}.y, {});",
                            word(unit * 4)
                        );
                    }
                    let _ = writeln!(
                        out,
                        "{indent}uint w{first}_{count} = 0u;
{indent}if ((a{first}.x & 3u) != 0u) w{first}_{count} = mem_load32_aligned_at(a{first}.x, a{first}.y, {});",
                        word(count * 4)
                    );
                    for unit in 0..count {
                        let inside = if unit == 0 {
                            format!("a{first}.z")
                        } else {
                            let offset = self.offset(position.offset, unit * 4);
                            format!("buf_locate({descriptor}, {index}, {offset}).z")
                        };
                        let _ = writeln!(
                            out,
                            "{indent}uint t{} = buf_pick({inside}, a{first}.x, w{first}_{unit}, w{first}_{});",
                            first + unit,
                            unit + 1
                        );
                    }
                    return Ok(());
                }
                for unit in 0..count {
                    let offset = self.offset(position.offset, unit * u32::from(*width / 8));
                    let _ = writeln!(
                        out,
                        "{indent}uint t{} = buf_load{width}({descriptor}, {index}, {offset});",
                        first + unit
                    );
                }
                return Ok(());
            }
            Op::StoreBuffer {
                descriptor,
                position,
                width,
                values,
                guard,
            } => {
                self.need("buf_store");
                let descriptor = self.descriptor(descriptor);
                let index = self.v(position.index);
                let mask = word(mask_of(*width));
                let _ = writeln!(out, "{indent}if ({}) {{", self.v(*guard));
                for (unit, value) in values.iter().enumerate() {
                    let offset = self.offset(position.offset, unit as u32 * u32::from(*width / 8));
                    let _ = writeln!(
                        out,
                        "{indent}    buf_store({descriptor}, {index}, {offset}, {}, {mask});",
                        self.v(*value)
                    );
                }
                let _ = writeln!(out, "{indent}}}");
                return Ok(());
            }
            Op::LoadFormat {
                descriptor,
                position,
                format,
            } => {
                if let Value::Word(code) = *format
                    && BufferFormat::from_code(code).is_none()
                {
                    return self
                        .unsupported("LoadFormat", "a buffer format this crate does not know");
                }
                self.need("fmt_load");
                let _ = writeln!(
                    out,
                    "{indent}uint4 q{first} = fmt_load({}, {}, {}, {});",
                    self.descriptor(descriptor),
                    self.v(position.index),
                    self.v(position.offset),
                    self.v(*format)
                );
                for channel in 0..4 {
                    let _ = writeln!(
                        out,
                        "{indent}uint t{} = q{first}.{};",
                        first + channel,
                        CHANNELS[channel as usize]
                    );
                }
                return Ok(());
            }
            Op::StoreFormat {
                descriptor,
                position,
                format,
                values,
                guard,
            } => {
                if let Value::Word(code) = *format {
                    let Some(known) = BufferFormat::from_code(code) else {
                        return self.unsupported(
                            "StoreFormat",
                            "a buffer format this crate does not know",
                        );
                    };
                    if known.encode([0; 4]).is_none() {
                        return self.unsupported(
                            "StoreFormat",
                            "a buffer format with 10-bit or 11-bit floats, which is not written",
                        );
                    }
                }
                self.need("fmt_store");
                let _ = writeln!(
                    out,
                    "{indent}if ({}) fmt_store({}, {}, {}, {}, uint4({}, {}, {}, {}));",
                    self.v(*guard),
                    self.descriptor(descriptor),
                    self.v(position.index),
                    self.v(position.offset),
                    self.v(*format),
                    self.v(values[0]),
                    self.v(values[1]),
                    self.v(values[2]),
                    self.v(values[3])
                );
                return Ok(());
            }
            Op::LoadCode { offset, words } => {
                for unit in 0..u32::from(*words) {
                    let value = match *offset {
                        // A table read at a constant place is its words.
                        Value::Word(offset) => word(
                            self.function
                                .code
                                .get((offset / 4) as usize + unit as usize)
                                .copied()
                                .unwrap_or(0),
                        ),
                        _ => {
                            self.code_table = true;
                            format!("code_word({})", self.offset(*offset, unit * 4))
                        }
                    };
                    let _ = writeln!(out, "{indent}uint t{} = {value};", first + unit);
                }
                return Ok(());
            }
            Op::Image(op) => {
                let built = image::function(op, self.stage())?;
                for helper in &built.helpers {
                    self.need(helper);
                }
                let index = match self.images.iter().position(|known| *known == built.text) {
                    Some(index) => index,
                    None => {
                        self.images.push(built.text.clone());
                        self.images.len() - 1
                    }
                };
                let mut arguments = Vec::new();
                for argument in &built.arguments {
                    arguments.push(match argument {
                        Argument::Value(value) => self.v(*value),
                        Argument::Sampler => match op.sampler {
                            Some(sampler) => self.sampler(sampler)?,
                            None => word(0),
                        },
                    });
                }
                let call = format!("image_{index}({})", arguments.join(", "));
                if matches!(op.kind, ImageKind::Store { .. }) {
                    let _ = writeln!(out, "{indent}{call};");
                } else {
                    let _ = writeln!(out, "{indent}uint4 q{first} = {call};");
                    for channel in 0..4 {
                        let _ = writeln!(
                            out,
                            "{indent}uint t{} = q{first}.{};",
                            first + channel,
                            CHANNELS[channel as usize]
                        );
                    }
                }
                return Ok(());
            }
            Op::Export {
                target,
                values,
                guard,
            } => return self.export(*target, values, *guard, indent, out),
            Op::Discard(condition) => {
                if !pixel {
                    return self.unsupported("Discard", "only a pixel shader discards");
                }
                match condition {
                    Value::Flag(false) => {}
                    Value::Flag(true) => {
                        let _ = writeln!(out, "{indent}discard;");
                    }
                    _ => {
                        let _ = writeln!(out, "{indent}if ({}) discard;", self.v(*condition));
                    }
                }
                return Ok(());
            }
            Op::FirstLane { value, active } => {
                self.need("wave_first");
                format!("wave_first({}, {})", self.v(*value), self.v(*active))
            }
            Op::ReadLane { value, lane } => {
                // A lane number written into the code is the console's: the
                // host numbers its lanes its own way.
                if lane.as_word().is_some() {
                    return self.unsupported(
                        "ReadLane",
                        "a read of a lane by a constant number, which is the console's numbering",
                    );
                }
                self.need("wave_read");
                format!("wave_read({}, {})", self.v(*value), self.v(*lane))
            }
            Op::CountLanes(mask) => {
                self.need("wave_count");
                format!("wave_count({})", self.v(*mask))
            }
            Op::FirstLaneIndex(mask) => {
                self.need("wave_first_index");
                format!("wave_first_index({})", self.v(*mask))
            }
            Op::AnyLane(mask) => {
                self.need("wave_any");
                format!("wave_any({})", self.v(*mask))
            }
            Op::LanesBelow { mask, half } => {
                self.need("wave_below");
                let half = match half {
                    None => 0,
                    Some(false) => 1,
                    Some(true) => 2,
                };
                format!("wave_below({}, {half}u)", self.v(*mask))
            }
            Op::BallotWord { mask, high } => {
                self.need("wave_ballot");
                format!("wave_ballot({}, {high})", self.v(*mask))
            }
            Op::LaneBit { low, high } => {
                if let (Some(low), Some(high)) = (low.as_word(), high.as_word()) {
                    // A lane number written into the code is the console's.
                    // In a pixel program four lanes in a row are a quad, so
                    // a pattern that repeats every four lanes says the same
                    // of a lane whatever its number: its bit is that of the
                    // lane's place in its quad.
                    match self.quad_pattern(low, high) {
                        Some(pattern) if pixel => {
                            format!("(({}u >> in_quad) & 1u) != 0u", pattern)
                        }
                        _ => {
                            return self.unsupported(
                                "LaneBit",
                                "the lane's bit of a constant, which is the console's numbering",
                            );
                        }
                    }
                } else {
                    self.need("lane_bit");
                    format!("lane_bit({}, {})", self.v(*low), self.v(*high))
                }
            }
            Op::QuadPermute {
                value,
                lanes,
                active,
            } => {
                if !pixel {
                    return self.unsupported(
                        "QuadPermute",
                        "a read across a quad outside a pixel shader, where the host has no quads",
                    );
                }
                if lanes.iter().any(|&lane| lane > 3) {
                    return self.unsupported("QuadPermute", "a place past the quad's fourth");
                }
                self.need("quad_read");
                // Which way each place of the quad looks: the source place
                // differs from the lane's own in x (1), in y (2) or both.
                let across: Vec<String> = lanes
                    .iter()
                    .enumerate()
                    .map(|(place, &lane)| word(u32::from(lane) ^ place as u32))
                    .collect();
                format!(
                    "quad_read(select({}, {}, 0u), uint4({})[in_quad])",
                    self.v(*active),
                    self.v(*value),
                    across.join(", ")
                )
            }
            Op::QuadAny(mask) => {
                if !pixel {
                    return self.unsupported(
                        "QuadAny",
                        "a test across a quad outside a pixel shader, where the host has no quads",
                    );
                }
                self.need("quad_any");
                format!("quad_any({})", self.v(*mask))
            }
        };
        let _ = writeln!(out, "{indent}{} t{first} = {expression};", ty(self));
        Ok(())
    }

    /// Writes an export.
    fn export(
        &mut self,
        target: ExportTarget,
        values: &[Option<Value>; 4],
        guard: Value,
        indent: &str,
        out: &mut String,
    ) -> Result<(), EmitError> {
        let stage = self.stage();
        let mut lines = Vec::new();
        // Assigns the exported channels of a four-word output.
        let four = |emitter: &Self, name: &str, convert: (&str, &str), lines: &mut Vec<String>| {
            if let [Some(a), Some(b), Some(c), Some(d)] = values {
                lines.push(format!(
                    "{name} = {}uint4({}, {}, {}, {}){};",
                    convert.0,
                    emitter.v(*a),
                    emitter.v(*b),
                    emitter.v(*c),
                    emitter.v(*d),
                    convert.1
                ));
            } else {
                for (channel, value) in values.iter().enumerate() {
                    if let Some(value) = value {
                        lines.push(format!(
                            "{name}.{} = {}{}{};",
                            CHANNELS[channel],
                            convert.0,
                            emitter.v(*value),
                            convert.1
                        ));
                    }
                }
            }
        };
        match (stage, target) {
            (HardwareStage::Pixel, ExportTarget::Color(n)) => {
                let Some(kind) = self.options.targets.get(usize::from(n)) else {
                    return self.unsupported("Export", "a colour target past the eighth");
                };
                let convert = match kind {
                    TargetKind::Float => ("asfloat(", ")"),
                    TargetKind::Unsigned => ("", ""),
                    TargetKind::Signed => ("(int4)", ""),
                };
                if let [Some(_), Some(_), Some(_), Some(_)] = values {
                    four(self, &format!("out_target{n}"), convert, &mut lines);
                } else {
                    // One channel at a time, a scalar conversion each.
                    let convert = match kind {
                        TargetKind::Float => ("asfloat(", ")"),
                        TargetKind::Unsigned => ("", ""),
                        TargetKind::Signed => ("(int)", ""),
                    };
                    four(self, &format!("out_target{n}"), convert, &mut lines);
                }
            }
            (HardwareStage::Pixel, ExportTarget::Depth) => {
                if let Some(depth) = values[0] {
                    lines.push(format!("out_depth = asfloat({});", self.v(depth)));
                }
                if let Some(stencil) = values[1] {
                    lines.push(format!("out_stencil = {};", self.v(stencil)));
                }
                if let Some(coverage) = values[2] {
                    lines.push(format!("out_coverage = {};", self.v(coverage)));
                }
            }
            (HardwareStage::Vertex, ExportTarget::Position(0)) => {
                four(self, "out_position", ("asfloat(", ")"), &mut lines);
            }
            (HardwareStage::Vertex, ExportTarget::Position(n @ (1 | 2))) => {
                // Clip distances and then cull distances, four to an
                // export; what the signature does not declare is dropped.
                for (channel, value) in values.iter().enumerate() {
                    let Some(value) = value else { continue };
                    let index = (u32::from(n) - 1) * 4 + channel as u32;
                    let (name, index, count) = if index < self.distances[0] {
                        ("out_clip", index, self.distances[0])
                    } else if index < self.distances[0] + self.distances[1] {
                        ("out_cull", index - self.distances[0], self.distances[1])
                    } else {
                        continue;
                    };
                    let in_field = count.min(8) - (index / 4) * 4;
                    let component = if in_field.min(4) == 1 {
                        String::new()
                    } else {
                        format!(".{}", CHANNELS[(index % 4) as usize])
                    };
                    if index < 8 {
                        lines.push(format!(
                            "{name}{}{component} = asfloat({});",
                            index / 4,
                            self.v(*value)
                        ));
                    }
                }
            }
            (HardwareStage::Vertex, ExportTarget::Parameter(n)) => {
                if n >= 32 {
                    return self.unsupported("Export", "a parameter past the thirty-second");
                }
                four(self, &format!("out_row{n}"), ("asfloat(", ")"), &mut lines);
            }
            _ => {
                return self.unsupported("Export", "an export the stage does not have");
            }
        }
        match guard {
            Value::Flag(true) => {
                for line in lines {
                    let _ = writeln!(out, "{indent}{line}");
                }
            }
            Value::Flag(false) => {}
            _ => {
                let _ = writeln!(out, "{indent}if ({}) {{", self.v(guard));
                for line in lines {
                    let _ = writeln!(out, "{indent}    {line}");
                }
                let _ = writeln!(out, "{indent}}}");
            }
        }
        Ok(())
    }

    /// Writes the instructions of a block.
    ///
    /// Two things keep the text near what a person would write. A read of a
    /// variable gets no line: the temporary is written as the variable's
    /// name wherever it is used, which is sound as long as the variable is
    /// not written between the read and the last use; where it is, the
    /// read keeps its line. And a result whose only use is the write to a
    /// variable right after it is written to the variable directly.
    fn block(&mut self, block: BlockId, indent: &str, out: &mut String) -> Result<(), EmitError> {
        let function = self.function;
        let block = &function.blocks[block];
        let insts = &block.insts;
        // Where each temporary is last used, and how often. The branch at
        // the end counts as a use after every instruction.
        let mut last_use: BTreeMap<u32, usize> = BTreeMap::new();
        let mut uses: BTreeMap<u32, u32> = BTreeMap::new();
        for (index, inst) in insts.iter().enumerate() {
            inst.op.for_each_operand(|value| {
                if let Value::Temp(temp) = value {
                    last_use.insert(temp.0, index);
                    *uses.entry(temp.0).or_default() += 1;
                }
            });
        }
        if let Terminator::Branch {
            condition: Value::Temp(temp),
            ..
        } = block.terminator
        {
            last_use.insert(temp.0, insts.len());
            *uses.entry(temp.0).or_default() += 1;
        }
        self.names.clear();
        let mut index = 0;
        while let Some(inst) = insts.get(index) {
            let first = inst.result.0;
            match &inst.op {
                Op::Get(var) => {
                    let until = last_use.get(&first).copied().unwrap_or(index);
                    let written = insts[index + 1..until.min(insts.len())]
                        .iter()
                        .any(|later| matches!(later.op, Op::Set(target, _) if target == *var));
                    if written {
                        self.inst(inst, indent, out)?;
                    } else {
                        self.names.insert(first, Self::var(*var));
                    }
                }
                op if op.result_count() == 1
                    && uses.get(&first) == Some(&1)
                    && matches!(
                        insts.get(index + 1).map(|next| &next.op),
                        Some(Op::Set(_, Value::Temp(temp))) if temp.0 == first
                    ) =>
                {
                    // Written as a declaration, then turned into the
                    // assignment the next instruction is.
                    let mut line = String::new();
                    self.inst(inst, "", &mut line)?;
                    let declared = format!("t{first} = ");
                    let target = match insts[index + 1].op {
                        Op::Set(var, _) => Self::var(var),
                        _ => String::new(),
                    };
                    match line.split_once(&declared) {
                        Some((_, expression)) if line.lines().count() == 1 => {
                            let _ = write!(out, "{indent}{target} = {expression}");
                            index += 1;
                        }
                        _ => {
                            for line in line.lines() {
                                let _ = writeln!(out, "{indent}{line}");
                            }
                        }
                    }
                }
                _ => self.inst(inst, indent, out)?,
            }
            index += 1;
        }
        Ok(())
    }

    /// Writes a jump to the scope at depth `to`.
    fn jump(&mut self, to: usize, indent: &str, out: &mut String) {
        let innermost = self.scopes.len() - 1;
        if to == innermost {
            let word = if self.scopes[to].is_loop {
                "continue"
            } else {
                "break"
            };
            let _ = writeln!(out, "{indent}{word};");
            return;
        }
        self.uses_go = true;
        self.scopes[to].lands = true;
        for scope in &mut self.scopes[to + 1..] {
            scope.passes.insert(to);
        }
        let _ = writeln!(out, "{indent}go = {}u; break;", self.scopes[to].id);
    }

    /// Writes what follows the end of a scope that was at depth `depth`:
    /// where a jump headed for it arrives, and where jumps that pass
    /// through go on.
    fn after(&mut self, scope: &Scope, depth: usize, indent: &str, out: &mut String) {
        if scope.lands && !scope.is_loop {
            let _ = writeln!(out, "{indent}if (go == {}u) go = 0u;", scope.id);
        }
        if scope.passes.is_empty() || depth == 0 {
            return;
        }
        let parent = &self.scopes[depth - 1];
        let mut onward = scope.passes.iter().any(|&to| to + 1 < depth);
        if scope.passes.contains(&(depth - 1)) {
            if parent.is_loop {
                let _ = writeln!(
                    out,
                    "{indent}if (go == {}u) {{ go = 0u; continue; }}",
                    parent.id
                );
            } else {
                onward = true;
            }
        }
        if onward {
            let _ = writeln!(out, "{indent}if (go != 0u) break;");
        }
    }

    fn open(&mut self, block: BlockId, is_loop: bool) {
        let id = self.next_scope;
        self.next_scope += 1;
        self.scopes.push(Scope {
            block,
            is_loop,
            id,
            lands: false,
            passes: BTreeSet::new(),
        });
    }

    /// Writes structured code.
    fn nodes(&mut self, nodes: &[Node], depth: usize, out: &mut String) -> Result<(), EmitError> {
        let indent = "    ".repeat(depth);

        for node in nodes {
            match node {
                Node::Code(block) => self.block(*block, &indent, out)?,
                Node::If {
                    condition,
                    negate,
                    then,
                } => {
                    let not = if *negate { "!" } else { "" };
                    let _ = writeln!(out, "{indent}if ({not}{}) {{", self.v(*condition));
                    self.nodes(then, depth + 1, out)?;
                    let _ = writeln!(out, "{indent}}}");
                }
                Node::Scope { follow, body } => {
                    self.open(*follow, false);
                    let mut inner = String::new();
                    // A jump out of the scope at its very end is what
                    // running off the end does anyway.
                    let body = match body.split_last() {
                        Some((Node::Break(to), rest)) if to == follow => rest,
                        _ => body,
                    };
                    self.nodes(body, depth + 1, &mut inner)?;
                    let scope = self.scopes.pop().ok_or(EmitError {
                        op: "the control flow",
                        reason: "a scope ends that never began",
                    })?;
                    let _ = writeln!(out, "{indent}do {{");
                    out.push_str(&inner);
                    let _ = writeln!(out, "{indent}}} while (false);");
                    let at = self.scopes.len();
                    self.after(&scope, at, &indent, out);
                }
                Node::Loop { header, body } => {
                    self.open(*header, true);
                    self.uses_fuel = true;
                    let mut inner = String::new();
                    self.nodes(body, depth + 1, &mut inner)?;
                    let scope = self.scopes.pop().ok_or(EmitError {
                        op: "the control flow",
                        reason: "a scope ends that never began",
                    })?;
                    let _ = writeln!(out, "{indent}for (; fuel != 0u; fuel--) {{");
                    out.push_str(&inner);
                    let _ = writeln!(out, "{indent}}}");
                    let at = self.scopes.len();
                    self.after(&scope, at, &indent, out);
                    // Only a lane that ran out of trips comes here.
                    let _ = writeln!(out, "{indent}return;");
                }
                Node::Break(follow) | Node::Continue(follow) => {
                    let is_loop = matches!(node, Node::Continue(_));
                    let Some(to) = self
                        .scopes
                        .iter()
                        .rposition(|scope| scope.block == *follow && scope.is_loop == is_loop)
                    else {
                        return self
                            .unsupported("the control flow", "a jump to a scope it is not inside");
                    };
                    self.jump(to, &indent, out);
                }
                Node::Return => {
                    let _ = writeln!(out, "{indent}return;");
                }
            }
        }
        Ok(())
    }

    /// Writes the function as a loop around a `switch` on a block number:
    /// what is left when the graph cannot be structured.
    fn dispatch(&mut self, out: &mut String) -> Result<(), EmitError> {
        self.uses_fuel = true;
        let function = self.function;
        let _ = writeln!(out, "    uint at = 0u;");
        let _ = writeln!(out, "    for (; fuel != 0u; fuel--) {{");
        let _ = writeln!(out, "        switch (at) {{");
        for (id, block) in function.blocks.iter().enumerate() {
            let _ = writeln!(out, "            case {id}u: {{");
            self.block(id, "                ", out)?;
            match block.terminator {
                Terminator::Jump(to) => {
                    let _ = writeln!(out, "                at = {to}u;");
                    let _ = writeln!(out, "                break;");
                }
                Terminator::Branch {
                    condition,
                    if_true,
                    if_false,
                } => {
                    let _ = writeln!(
                        out,
                        "                at = {} ? {if_true}u : {if_false}u;",
                        self.v(condition)
                    );
                    let _ = writeln!(out, "                break;");
                }
                Terminator::Return => {
                    let _ = writeln!(out, "                return;");
                }
            }
            let _ = writeln!(out, "            }}");
        }
        let _ = writeln!(out, "            default: return;");
        let _ = writeln!(out, "        }}");
        let _ = writeln!(out, "    }}");
        Ok(())
    }

    /// The table of buffer formats as an HLSL function, written from
    /// [`BufferFormat`] so that the two cannot disagree.
    fn format_table() -> String {
        let mut text = String::from(
            "// A buffer format by its number: the widths of x, y, z and w, a byte each,\n// and how a channel is read. Zeroes for a number that names no format.\nuint2 fmt_info(uint code) {\n    switch (code) {\n",
        );
        for code in 0..128u32 {
            let Some(format) = BufferFormat::from_code(code) else {
                continue;
            };
            let widths = u32::from_le_bytes(format.bits);
            let kind = match format.kind {
                ChannelKind::Unorm => 0,
                ChannelKind::Snorm => 1,
                ChannelKind::Uscaled => 2,
                ChannelKind::Sscaled => 3,
                ChannelKind::Uint => 4,
                ChannelKind::Sint => 5,
                ChannelKind::Float => 6,
            };
            let _ = writeln!(
                text,
                "        case {code}u: return uint2({widths:#010x}u, {kind}u);"
            );
        }
        text.push_str("        default: return uint2(0u, 0u);\n    }\n}\n");
        text
    }

    /// The helpers the shader uses and those they call, in order.
    fn helper_text(&self) -> String {
        let mut wanted: BTreeSet<&str> = self.helpers.iter().copied().collect();
        // A helper comes after what it needs, so one pass from the end
        // finds everything.
        for helper in HELPERS.iter().rev() {
            if wanted.contains(helper.name) {
                wanted.extend(helper.needs.iter().copied());
            }
        }
        let pixel = self.stage() == HardwareStage::Pixel;
        let mut text = String::new();
        for helper in HELPERS {
            if !wanted.contains(helper.name) {
                continue;
            }
            if helper.name == "fmt_info" {
                text.push_str(&Self::format_table());
                continue;
            }
            for line in helper.text.lines() {
                // Where a helper lane of a pixel shader leaves: the host's
                // wave operations do not see such lanes, so one answers
                // for itself alone.
                if let Some(own) = line
                    .trim()
                    .strip_prefix("HELPER(")
                    .and_then(|rest| rest.strip_suffix(");"))
                {
                    if pixel {
                        let _ = writeln!(text, "    if (IsHelperLane()) return {own};");
                    }
                } else {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
        text
    }

    /// Puts the shader together.
    fn finish(mut self) -> Result<Emitted, EmitError> {
        let function = self.function;
        let stage = self.stage();
        let flow = ControlFlow::of(function);
        let mut body = String::new();
        let dispatched = match structure(function, &flow) {
            Some(nodes) => {
                self.nodes(&nodes, 1, &mut body)?;
                false
            }
            None => {
                self.dispatch(&mut body)?;
                true
            }
        };

        let space = match stage {
            HardwareStage::Vertex => 0,
            HardwareStage::Pixel => 1,
            _ => 2,
        };
        let mut source = String::new();
        let _ = writeln!(
            source,
            "// Written by riptide-shader {} from a console {} program.",
            super::VERSION,
            stage.short_name()
        );
        let _ = writeln!(
            source,
            "cbuffer Inputs : register(b0, space{space}) {{ uint4 input_words[20]; }}"
        );
        if self.code_table {
            let words: Vec<String> = function.code.iter().map(|&value| word(value)).collect();
            let count = words.len().max(1);
            let _ = writeln!(
                source,
                "// The program's own code, where its compiler kept tables of constants.\nstatic const uint code_words[{count}] = {{ {} }};\nuint code_word(uint offset) {{ uint index = offset >> 2; return index < {}u ? code_words[min(index, {}u)] : 0u; }}",
                if words.is_empty() {
                    "0u".to_string()
                } else {
                    words.join(", ")
                },
                words.len(),
                count - 1
            );
        }
        // The stage's own text first, since it decides which helpers are
        // needed (a per-vertex term needs a subtraction).
        let (declarations, main, rows, flat_rows) = match stage {
            HardwareStage::Vertex => self.vertex_stage(),
            HardwareStage::Pixel => self.pixel_stage(),
            _ => self.compute_stage(),
        };
        source.push_str(&self.helper_text());
        source.push_str(&declarations);
        for (index, text) in self.images.iter().enumerate() {
            source.push_str(&text.replacen("IMAGE", &format!("image_{index}"), 1));
        }

        source.push_str("\nvoid program() {\n");
        for var in function.variables() {
            let (ty, start) = match (var.ty(), var) {
                (Ty::Flag, Var::Exec) => ("bool", "true"),
                (Ty::Flag, _) => ("bool", "false"),
                (Ty::Word, _) => ("uint", "0u"),
            };
            let _ = writeln!(source, "    {ty} {} = {start};", Self::var(var));
        }
        if self.uses_go {
            source.push_str("    uint go = 0u;\n");
        }
        if self.uses_fuel {
            let _ = writeln!(source, "    uint fuel = {}u;", fuel(stage));
        }
        source.push_str(&body);
        source.push_str("}\n\n");
        source.push_str(&main);
        Ok(Emitted {
            source,
            samplers: self.samplers,
            rows,
            flat_rows,
            dispatched,
        })
    }

    /// The declarations and `main` of a vertex shader, and its row count.
    fn vertex_stage(&mut self) -> (String, String, u32, u32) {
        let mut declarations = String::new();
        let mut fields = Vec::new();
        let mut prologue = String::new();
        if let Some(inputs) = self.signatures.inputs {
            for &index in &self.uses.elements {
                let element = &inputs.elements[index];
                let ty = self.element_type(index);
                fields.push(format!(
                    "    {ty} a{index} : {}{};",
                    element.semantic_name, element.semantic_index
                ));
                let _ = writeln!(declarations, "static {ty} in_a{index};");
                let _ = writeln!(prologue, "    in_a{index} = input.a{index};");
            }
        }
        // The console counts a vertex from the draw's first vertex and an
        // instance from its first instance; the host counts both from zero.
        if self.uses.vertex_id {
            fields.push("    uint vertex : SV_VertexID;".to_string());
            declarations.push_str("static uint in_vertex;\n");
            prologue.push_str("    in_vertex = input.vertex + input_words[19].w;\n");
        }
        if self.uses.instance_id {
            fields.push("    uint instance : SV_InstanceID;".to_string());
            declarations.push_str("static uint in_instance;\n");
            prologue.push_str("    in_instance = input.instance + input_words[19].z;\n");
        }
        let rows = self
            .uses
            .parameters
            .iter()
            .next_back()
            .map_or(0, |&highest| u32::from(highest) + 1);
        let mut outputs = Vec::new();
        for row in 0..rows {
            outputs.push((
                format!("float4 row{row} : ROW{row}"),
                format!("out_row{row}"),
            ));
        }
        outputs.push((
            "float4 position : SV_Position".to_string(),
            "out_position".to_string(),
        ));
        for (name, semantic, count) in [
            ("clip", "SV_ClipDistance", self.distances[0]),
            ("cull", "SV_CullDistance", self.distances[1]),
        ] {
            let count = count.min(8);
            for field in 0..count.div_ceil(4) {
                let width = (count - field * 4).min(4);
                let ty = if width == 1 {
                    "float".to_string()
                } else {
                    format!("float{width}")
                };
                outputs.push((
                    format!("{ty} {name}{field} : {semantic}{field}"),
                    format!("out_{name}{field}"),
                ));
            }
        }
        if !fields.is_empty() {
            let _ = writeln!(
                declarations,
                "struct VertexIn {{\n{}\n}};",
                fields.join("\n")
            );
        }
        declarations.push_str("struct VertexOut {\n");
        for (field, _) in &outputs {
            let _ = writeln!(declarations, "    {field};");
        }
        declarations.push_str("};\n");
        for (field, name) in &outputs {
            let ty = field.split(' ').next().unwrap_or("float4");
            let _ = writeln!(declarations, "static {ty} {name};");
        }
        let mut main = String::new();
        let parameter = if fields.is_empty() {
            ""
        } else {
            "VertexIn input"
        };
        let _ = writeln!(main, "VertexOut main({parameter}) {{");
        main.push_str(&prologue);
        for (field, name) in &outputs {
            let ty = field.split(' ').next().unwrap_or("float4");
            let _ = writeln!(main, "    {name} = ({ty})0;");
        }
        main.push_str("    program();\n    VertexOut output;\n");
        for (field, name) in &outputs {
            let member = field.split(' ').nth(1).unwrap_or("position");
            let _ = writeln!(main, "    output.{member} = {name};");
        }
        main.push_str("    return output;\n}\n");
        (declarations, main, rows, 0)
    }

    /// The declarations and `main` of a pixel shader, how many rows it
    /// declares, and which of them must not be interpolated.
    fn pixel_stage(&mut self) -> (String, String, u32, u32) {
        let mut statics = String::new();
        let mut fields = Vec::new();
        let mut prologue = String::new();
        let rows = self
            .uses
            .rows
            .keys()
            .next_back()
            .map_or(0, |&highest| u32::from(highest) + 1);
        let mut flat_rows = 0u32;
        let mut sample = self.uses.sample;
        // How a value declared at the pixel's centre is taken elsewhere.
        let moved = |index: usize, name: &str, sample: &mut bool| match index % 3 {
            0 => {
                *sample = true;
                format!("EvaluateAttributeAtSample({name}, input.sample_index)")
            }
            2 => format!("EvaluateAttributeCentroid({name})"),
            _ => name.to_string(),
        };
        for row in 0..rows {
            let uses = self
                .uses
                .rows
                .get(&(row as u8))
                .cloned()
                .unwrap_or_default();
            // The values at the vertices that are read: all three to weigh a
            // row by hand, else the first and those a difference is taken of.
            let wanted = [
                true,
                uses.terms[1] || uses.by_hand(),
                uses.terms[2] || uses.by_hand(),
            ];
            // A row that is only read at its first vertex is what a row
            // that is not interpolated holds anyway; that needs nothing of
            // the host, where reading the other vertices needs a device
            // that hands out per-vertex values.
            let only_first = !wanted[1] && !wanted[2];
            let vertices = |statics: &mut String, prologue: &mut String| {
                for vertex in (0..3).filter(|&vertex| wanted[vertex]) {
                    let _ = writeln!(statics, "static float4 in_row{row}_v{vertex};");
                    if only_first {
                        let _ = writeln!(prologue, "    in_row{row}_v0 = input.row{row};");
                    } else {
                        let _ = writeln!(
                            prologue,
                            "    in_row{row}_v{vertex} = GetAttributeAtVertex(input.row{row}, {vertex});"
                        );
                    }
                }
            };
            if uses.by_hand() {
                // Read both ways: the three vertices' values, weighed here.
                fields.push(format!("    nointerpolation float4 row{row} : ROW{row};"));
                vertices(&mut statics, &mut prologue);
                for &index in &uses.interpolated {
                    let name = WEIGHT_NAMES[index];
                    let _ = writeln!(statics, "static float4 in_row{row}_{name};");
                    let _ = writeln!(
                        prologue,
                        "    in_row{row}_{name} = in_weights_{name}.x * in_row{row}_v0 + in_weights_{name}.y * in_row{row}_v1 + in_weights_{name}.z * in_row{row}_v2;"
                    );
                }
            } else if uses.any_term() {
                fields.push(format!("    nointerpolation float4 row{row} : ROW{row};"));
                vertices(&mut statics, &mut prologue);
                if !uses.terms[1] && !uses.terms[2] {
                    flat_rows |= 1 << row;
                }
            } else if uses.interpolated.len() == 1 {
                let index = uses.interpolated.iter().next().copied().unwrap_or(1);
                let name = WEIGHT_NAMES[index];
                fields.push(format!(
                    "    {}float4 row{row} : ROW{row};",
                    WEIGHT_MODIFIERS[index]
                ));
                let _ = writeln!(statics, "static float4 in_row{row}_{name};");
                let _ = writeln!(prologue, "    in_row{row}_{name} = input.row{row};");
            } else {
                // Not read, or read at several places of the pixel: declared
                // at the centre and moved.
                let linear = uses.interpolated.iter().any(|&index| is_linear(index));
                fields.push(format!(
                    "    {}float4 row{row} : ROW{row};",
                    if linear { "noperspective " } else { "" }
                ));
                for &index in &uses.interpolated {
                    let name = WEIGHT_NAMES[index];
                    let _ = writeln!(statics, "static float4 in_row{row}_{name};");
                    let value = moved(index, &format!("input.row{row}"), &mut sample);
                    let _ = writeln!(prologue, "    in_row{row}_{name} = {value};");
                }
            }
        }
        if self.uses.position {
            fields.push("    float4 position : SV_Position;".to_string());
            statics.push_str("static float4 in_position;\n");
            prologue.push_str("    in_position = input.position;\n");
        }
        if self.uses.quad {
            statics.push_str("// The lane's place in its quad: 0 top left, 1 top right, 2 bottom left, 3 bottom right.\nstatic uint in_quad;\n");
            prologue.push_str(
                "    in_quad = ((uint)input.position.x & 1u) | (((uint)input.position.y & 1u) << 1);\n",
            );
        }
        if self.uses.front {
            fields.push("    bool front : SV_IsFrontFace;".to_string());
            statics.push_str("static bool in_front;\n");
            prologue.push_str("    in_front = input.front;\n");
        }
        if self.uses.coverage {
            fields.push("    uint coverage : SV_Coverage;".to_string());
            statics.push_str("static uint in_coverage;\n");
            prologue.push_str("    in_coverage = input.coverage;\n");
        }
        // The weights as numbers: the host's barycentric coordinates, of
        // which the second and third weigh vertices 1 and 2 as the
        // console's i and j do.
        let mut weights_prologue = String::new();
        let mut declared = [false; 2];
        for &index in &self.uses.weights {
            let linear = is_linear(index);
            let field = if linear { "weights_flat" } else { "weights" };
            if !declared[usize::from(linear)] {
                declared[usize::from(linear)] = true;
                let number = usize::from(linear && declared[0]);
                fields.push(format!(
                    "    {}float3 {field} : SV_Barycentrics{number};",
                    if linear { "noperspective " } else { "" }
                ));
            }
            let name = WEIGHT_NAMES[index];
            let _ = writeln!(statics, "static float3 in_weights_{name};");
            let value = moved(index, &format!("input.{field}"), &mut sample);
            let _ = writeln!(weights_prologue, "    in_weights_{name} = {value};");
        }
        // The weights are worked out before the rows that are weighed by
        // hand.
        prologue.insert_str(0, &weights_prologue);
        if sample {
            fields.push("    uint sample_index : SV_SampleIndex;".to_string());
            if self.uses.sample {
                statics.push_str("static uint in_sample;\n");
                prologue.push_str("    in_sample = input.sample_index;\n");
            }
        }

        let mut outputs = Vec::new();
        for &target in &self.uses.colors {
            let ty = match self.options.targets.get(usize::from(target)) {
                Some(TargetKind::Unsigned) => "uint4",
                Some(TargetKind::Signed) => "int4",
                _ => "float4",
            };
            outputs.push((
                format!("{ty} target{target} : SV_Target{target}"),
                format!("out_target{target}"),
            ));
        }
        if self.uses.depth[0] {
            outputs.push((
                "float depth : SV_Depth".to_string(),
                "out_depth".to_string(),
            ));
        }
        if self.uses.depth[1] {
            outputs.push((
                "uint stencil : SV_StencilRef".to_string(),
                "out_stencil".to_string(),
            ));
        }
        if self.uses.depth[2] {
            outputs.push((
                "uint coverage : SV_Coverage".to_string(),
                "out_coverage".to_string(),
            ));
        }

        let mut declarations = String::new();
        if !fields.is_empty() {
            let _ = writeln!(
                declarations,
                "struct PixelIn {{\n{}\n}};",
                fields.join("\n")
            );
        }
        declarations.push_str(&statics);
        if !outputs.is_empty() {
            declarations.push_str("struct PixelOut {\n");
            for (field, _) in &outputs {
                let _ = writeln!(declarations, "    {field};");
            }
            declarations.push_str("};\n");
        }
        for (field, name) in &outputs {
            let ty = field.split(' ').next().unwrap_or("float4");
            let _ = writeln!(declarations, "static {ty} {name};");
        }
        let mut main = String::new();
        let parameter = if fields.is_empty() {
            ""
        } else {
            "PixelIn input"
        };
        let result = if outputs.is_empty() {
            "void"
        } else {
            "PixelOut"
        };
        let _ = writeln!(main, "{result} main({parameter}) {{");
        main.push_str(&prologue);
        for (field, name) in &outputs {
            let ty = field.split(' ').next().unwrap_or("float4");
            let _ = writeln!(main, "    {name} = ({ty})0;");
        }
        main.push_str("    program();\n");
        if !outputs.is_empty() {
            main.push_str("    PixelOut output;\n");
            for (field, name) in &outputs {
                let member = field.split(' ').nth(1).unwrap_or("target0");
                let _ = writeln!(main, "    output.{member} = {name};");
            }
            main.push_str("    return output;\n");
        }
        main.push_str("}\n");
        (declarations, main, rows, flat_rows)
    }

    /// The declarations and `main` of a compute shader.
    fn compute_stage(&mut self) -> (String, String, u32, u32) {
        let mut declarations = String::new();
        let mut prologue = String::new();
        let mut parameters = Vec::new();
        if self.uses.thread {
            declarations.push_str("static uint3 in_thread;\n");
            parameters.push("uint3 thread : SV_GroupThreadID");
            prologue.push_str("    in_thread = thread;\n");
        }
        if self.uses.group {
            declarations.push_str("static uint3 in_group;\n");
            parameters.push("uint3 group : SV_GroupID");
            prologue.push_str("    in_group = group;\n");
        }
        let size = self.function.interface.group_size.map(|axis| axis.max(1));
        let main = format!(
            "[numthreads({}, {}, {})]\nvoid main({}) {{\n{prologue}    program();\n}}\n",
            size[0],
            size[1],
            size[2],
            parameters.join(", ")
        );
        (declarations, main, 0, 0)
    }
}

/// The bits a store of a unit of `width` bits writes.
fn mask_of(width: u8) -> u32 {
    match width {
        8 => 0xFF,
        16 => 0xFFFF,
        _ => u32::MAX,
    }
}
