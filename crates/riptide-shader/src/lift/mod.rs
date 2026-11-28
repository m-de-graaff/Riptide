//! The lift: decoded machine instructions to the intermediate form.
//!
//! [`lift`] takes one program and its stage data and gives a
//! [`Function`], or a [`LiftError`] that names the
//! instruction it could not lift and why. Nothing is ever lifted to a made-up
//! value: an instruction without a rule here fails the program.
//!
//! # How a wave program becomes a lane program
//!
//! The rules are those of the research note
//! (`docs/research/2026-11-28-console-shader-convention.md`, sections 7 and
//! 8), restated here because the lift's correctness rests on them:
//!
//! - **Vector registers** are words of the lane. A write under a live-lane
//!   mask that may be clear keeps the old value in a lane that is not live.
//! - **Scalar registers** hold numbers or lane masks, and the same register
//!   holds one at one time and the other later. The lift follows, along
//!   every path, which of the two forms each register holds
//!   (its `State`): a comparison, `EXEC` and `VCC` give masks; arithmetic,
//!   loads and inputs give numbers; a bitwise operation gives what its
//!   operands are; the constants 0 and -1 are both. A mask becomes the
//!   lane's own bit, a flag. Where a mask is read as a number (a bit count,
//!   a first set bit) or a number as a mask, the lift writes the explicit
//!   cross-lane operation for it. Where a register holds a mask on one path
//!   into a join and a number on another, both forms are made on the way
//!   in: the lane's own bit of the number on the one path, the lanes' bits
//!   gathered into a number on the other. Whatever reads the register after
//!   the join then finds the form it wants, and the form nothing reads is
//!   removed with the other dead writes.
//! - **Branches.** The console branches the whole wave: "no lane is live",
//!   "some lane's bit is set". The lane branches on its own bit. That is
//!   sound for code a compiler made, because such a branch only skips work
//!   that would not change a lane whose bit is clear; it is not sound for
//!   arbitrary machine code. This is taken from the research note, and the
//!   tests check it on the control-flow patterns compilers emit (if, else,
//!   loop, break), by running the lifted program against a wave-level
//!   interpreter of the machine code.
//! - **The condition code `SCC`** after an operation on masks says "the
//!   result is not zero", that is, some lane's bit is set. For a branch the
//!   lane uses its own bit. Anything else that reads it (a select, a carry)
//!   gets [`Op::AnyLane`] of it.
//! - **Whole-wave sections.** A wave reduction or prefix sum is written out
//!   by the console's compiler as code that runs in every lane and reads
//!   lanes by their numbers. Such a section is lifted as a whole, to
//!   [`Op::Reduce`] or [`Op::PrefixSum`]: see the `wave` module. Outside
//!   such a section a lane permutation that leaves the lane's group of four
//!   fails the program.
//! - **What the lanes share.** Atomic operations and the group-shared
//!   memory of a compute program are lifted to operations that say so
//!   ([`Op::Atomic`], [`Op::LoadShared`], [`Op::StoreShared`],
//!   [`Op::Barrier`]). The console does each for the whole wave at one
//!   instruction; the reference interpreter keeps that order, and an
//!   emitter has to make it hold on the host.
//!
//! # Inputs
//!
//! The entry block writes what the stage hands the program into its
//! registers: the user-data words of the root parameters, into the scalar
//! registers the root parameter table names (in the title's vertex and pixel
//! programs they start at `s8`); a pixel program's weights and position
//! into vector registers by the bits of `SPI_PS_INPUT_ADDR`; a vertex
//! program's vertex and instance numbers into `v0` and `v3`; a compute
//! program's thread position into `v0` to `v2` and its group position into
//! the scalar registers after the user data. A vertex program's attributes
//! arrive where it calls its fetch routine (`s_swappc_b64`): the call is
//! lifted to reads of [`Input::VertexAttribute`](crate::ir::Input) into the
//! registers the stage data names.
//!
//! # What is not lifted
//!
//! Mesh programs and the stages the title does not use; a compute program
//! whose parameters are spilled to memory; lane reads across groups of
//! four that are not part of a wave reduction; atomic operations on 64-bit
//! words, on floats and on images; 64-bit floats; and whatever
//! [`LiftError`] names. `examples/coverage.rs` counts these over a folder
//! of containers.

mod image;
mod memory;
mod park;
mod scalar;
mod vector;
mod wave;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::{BTreeMap, HashMap};

use crate::container::stage::{ExportFormat, PixelSystemInput, StageData};
use crate::container::xbox::{HardwareStage, Program};
use crate::gfx10::{Detail, Format, Half, Instruction, Operand, Role, WaveSize, walk};
use crate::ir::cfg::{ControlFlow, make_reducible, remove_dead};
use crate::ir::{
    AttributeRow, BinaryOp, Block, BlockId, CompareOp, ExportTarget, FlagOp, Function, Input, Inst,
    Interface, Op, ParameterRow, Temp, Terminator, UnaryOp, Value, Var, Weights, eval,
};

/// Why a program could not be lifted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiftError {
    /// The byte offset of the instruction in the program, if the failure is
    /// about one instruction.
    pub offset: Option<u32>,
    /// The instruction's mnemonic, or what was being lifted when the failure
    /// is not about one instruction.
    pub instruction: &'static str,
    /// What is wrong, in words that hold nothing of the program itself, so
    /// that failures can be counted and logged.
    pub reason: &'static str,
}

impl std::fmt::Display for LiftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.offset {
            Some(offset) => write!(
                f,
                "{} at {offset:#x} is not lifted: {}",
                self.instruction, self.reason
            ),
            None => write!(f, "{} is not lifted: {}", self.instruction, self.reason),
        }
    }
}

impl std::error::Error for LiftError {}

/// The result of lifting steps.
type Lift<T> = Result<T, LiftError>;

/// The register holds a number, in [`Var::Scalar`].
const NUM: u8 = 1;
/// The register is where a lane mask starts; the lane's bit is in
/// [`Var::Mask`] of this register.
const MASK: u8 = 2;
/// The register is the high half of the 64-lane mask that starts one below.
const HIGH: u8 = 4;
/// The mask that starts here is known to have the lane's bit set.
const TRUE: u8 = 8;
/// The number is a byte offset into the program's own code, from
/// `s_getpc_b64`; the register above it holds the high half of the address.
const CODE: u8 = 16;

/// The register holds what a whole-wave section left in it and the lift did
/// not follow (`lift/wave.rs`). It cannot be read.
const LEFT: u8 = 32;

/// How many scalar register codes the state follows: s0 to s105 and, at
/// the two codes after them, the two halves of `VCC`.
const SCALARS: usize = 108;

/// The place of `VCC` among the scalar registers the state follows. It
/// holds a lane mask, in [`Var::Vcc`], unless a program has run out of
/// scalar registers and keeps a number in it, which is then in
/// [`Var::Scalar`] of this number (and of the next for the high half).
const VCC: usize = 106;

/// The variable that holds the lane's bit of the mask that starts at a
/// register.
fn mask_var(register: u8) -> Var {
    if usize::from(register) == VCC {
        Var::Vcc
    } else {
        Var::Mask(register)
    }
}

/// What the condition code holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Condition {
    /// A flag that is the same in every lane.
    Uniform,
    /// The lane's bit of a mask; the console's flag is "some bit is set".
    LaneBit,
    /// Different things on different paths.
    Unknown,
}

/// How conversions to 16-bit floats round. Programs change it with a write
/// to the float mode register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rounding {
    /// To nearest, ties to even: the mode a program starts in.
    Nearest,
    /// Toward zero.
    TowardZero,
    /// Different modes on different paths.
    Unknown,
}

/// Whether a pixel program has passed its final export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exported {
    /// On every path here.
    Yes,
    /// On no path here.
    No,
    /// On some.
    Mixed,
}

/// What is known at one point of the program, the same along every path
/// that reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    /// For each scalar register, the forms it holds: bits [`NUM`], [`MASK`],
    /// [`HIGH`], [`TRUE`], [`CODE`], [`LEFT`]. No bit means the paths
    /// disagree.
    forms: [u8; SCALARS],
    /// For each scalar register, its number if that is a known constant.
    constants: [Option<u32>; SCALARS],
    /// What the condition code holds.
    condition: Condition,
    /// Whether the lane is known to be live.
    live: bool,
    /// Whether the final export has been passed.
    exported: Exported,
    /// How conversions to 16-bit floats round.
    half_rounding: Rounding,
    /// Which of a pixel program's weight registers still hold the weight
    /// the stage handed over: bit n for entry n of `Lifter::weights`.
    pristine: u16,
    /// The vector registers that hold what a whole-wave section left in
    /// them and the lift did not follow: bit n of word n / 64 for `v<n>`.
    left: [u64; 4],
    /// The scalar registers that hold a mask on one path here and a number
    /// on another, and for which both forms are made on the way in: bit n
    /// of word n / 64 for register n.
    both: [u64; 2],
}

impl State {
    /// The state at the start of a program.
    fn start() -> State {
        let mut forms = [NUM; SCALARS];
        forms[VCC] = MASK;
        forms[VCC + 1] = HIGH;
        State {
            forms,
            constants: [None; SCALARS],
            condition: Condition::Uniform,
            live: true,
            exported: Exported::No,
            half_rounding: Rounding::Nearest,
            pristine: u16::MAX,
            left: [0; 4],
            both: [0; 2],
        }
    }

    /// Whether both forms of a register are made on the way in.
    fn makes_both(&self, index: usize) -> bool {
        self.both[index / 64] >> (index % 64) & 1 != 0
    }

    /// Whether this state can give both forms of the lane mask that starts
    /// at a register: it holds the mask, or the number in as many
    /// registers as a mask takes. A number that is a known constant does
    /// not count: its bits would be read by the console's lane numbers,
    /// and where such a register meets a mask it is most often a constant
    /// some earlier code left behind.
    fn gives_both(&self, index: usize, wide: bool) -> bool {
        let high = if wide {
            self.forms[index + 1]
        } else {
            NUM | HIGH
        };
        let constant =
            self.constants[index].is_some() && (!wide || self.constants[index + 1].is_some());
        (self.forms[index] & MASK != 0 && high & HIGH != 0)
            || (self.forms[index] & NUM != 0 && high & NUM != 0 && !constant)
    }

    /// Narrows this state to what also holds in `other`, which comes in
    /// along another path. Returns whether anything changed. `wide` says a
    /// lane mask takes two registers.
    ///
    /// A register that holds a mask here and a number there (or the other
    /// way about) is not given up: from then on it holds both, and
    /// [`Lifter::convert`] makes the missing form at the end of each block
    /// that comes in, for as long as every such block has one of the two.
    fn meet(&mut self, other: &State, wide: bool) -> bool {
        let before = self.clone();
        for index in 0..SCALARS {
            self.forms[index] &= other.forms[index];
            if self.constants[index] != other.constants[index] {
                self.constants[index] = None;
            }
        }
        for index in 0..SCALARS - 1 {
            let (mine, theirs) = (before.forms[index], other.forms[index]);
            if before.makes_both(index) {
                // Decided earlier. The high half of a mask follows its
                // start, which is the register before.
                if mine & MASK != 0 || mine & HIGH == 0 {
                    let kept = mine & (NUM | MASK) != 0 && other.gives_both(index, wide);
                    self.forms[index] = if kept { NUM | MASK } else { 0 };
                    if wide && before.makes_both(index + 1) {
                        self.forms[index + 1] = if kept { NUM | HIGH } else { 0 };
                    }
                }
                continue;
            }
            let only = |forms: u8, wanted: u8| forms & (NUM | MASK) == wanted;
            let split =
                (only(mine, MASK) && only(theirs, NUM)) || (only(mine, NUM) && only(theirs, MASK));
            if split && before.gives_both(index, wide) && other.gives_both(index, wide) {
                self.forms[index] = NUM | MASK;
                self.both[index / 64] |= 1 << (index % 64);
                if wide {
                    self.forms[index + 1] = NUM | HIGH;
                    self.both[(index + 1) / 64] |= 1 << ((index + 1) % 64);
                }
            }
        }
        if self.condition != other.condition {
            self.condition = Condition::Unknown;
        }
        self.live &= other.live;
        if self.exported != other.exported {
            self.exported = Exported::Mixed;
        }
        if self.half_rounding != other.half_rounding {
            self.half_rounding = Rounding::Unknown;
        }
        self.pristine &= other.pristine;
        for (left, other) in self.left.iter_mut().zip(&other.left) {
            *left |= other;
        }
        *self != before
    }
}

/// How a machine block ends, with successors as machine block numbers.
enum End {
    /// Goes on to one block.
    Jump(usize),
    /// Goes to `if_true` when the flag holds.
    Branch {
        condition: Value,
        if_true: usize,
        if_false: usize,
    },
    /// The program ends.
    Return,
}

/// A lifted machine block before block numbers are final.
struct Lifted {
    insts: Vec<Inst>,
    end: End,
    /// The state on the way to each successor.
    out: Vec<(usize, State)>,
}

/// The lift of one program.
struct Lifter<'a> {
    stage: HardwareStage,
    data: &'a StageData,
    wave: WaveSize,
    /// The program's instructions in address order.
    instructions: Vec<Instruction>,
    /// The index in `instructions` of each machine block's first
    /// instruction.
    starts: Vec<usize>,
    /// The machine block that starts at a byte offset.
    block_at: HashMap<u32, usize>,
    /// Where each register jump recognised as a far jump goes, by the
    /// offset of the jump.
    far_jumps: HashMap<u32, u32>,
    /// The registers a pixel program's weights arrive in: register, which
    /// pair, and whether it is the second weight.
    weights: Vec<(u8, Weights, bool)>,
    /// How many temporaries have been handed out.
    temps: u32,
    /// The instructions of the block being lifted.
    insts: Vec<Inst>,
    /// What is known at the point being lifted.
    state: State,
    /// The instruction being lifted, for error messages.
    at: (Option<u32>, &'static str),
    /// Whether some end of a pixel program is reached both with and without
    /// the final export, so that a variable has to remember it.
    remember_export: bool,
    /// The second halves of interpolations whose first half, earlier in
    /// the block being lifted, was skipped: by index in `instructions`,
    /// with the weights the pair interpolates with.
    paired: HashMap<usize, Weights>,
}

/// Lifts one program.
///
/// A pixel program's colour exports are patched to four raw 32-bit values
/// ([`ExportFormat::Abgr32`]) first, as the design requires. The function
/// that comes back has a reducible control-flow graph and passes
/// [`Function::verify`].
pub fn lift(program: &Program, data: &StageData) -> Result<Function, LiftError> {
    let fail = |instruction, reason| LiftError {
        offset: None,
        instruction,
        reason,
    };
    let code = match (program.stage, data) {
        (HardwareStage::Pixel, StageData::Pixel(pixel)) => pixel
            .patched_code(&program.code, ExportFormat::Abgr32)
            .map_err(|_| fail("the program", "a colour export patch lies outside the code"))?,
        (HardwareStage::Vertex, StageData::Vertex(_))
        | (HardwareStage::Compute, StageData::Compute(_)) => program.code.clone(),
        (HardwareStage::Mesh, _) => {
            return Err(fail("the program", "mesh programs are not lifted"));
        }
        (HardwareStage::Vertex | HardwareStage::Pixel | HardwareStage::Compute, _) => {
            return Err(fail(
                "the program",
                "the stage data is not that of the program's stage",
            ));
        }
        _ => {
            return Err(fail("the program", "the stage is not lifted"));
        }
    };
    if let StageData::Compute(compute) = data {
        let needed = program
            .root_parameters
            .iter()
            .map(|parameter| u32::from(parameter.user_data) + parameter.register_count)
            .max()
            .unwrap_or(0);
        if needed > u32::from(compute.user_registers()) {
            return Err(fail(
                "the program",
                "a compute program whose parameters are spilled to memory",
            ));
        }
    }
    let walked = walk(&code, program.wave);
    if walked.stray_branches != 0 || walked.runs_off_end {
        return Err(fail("the program", "the decode does not follow the code"));
    }
    let mut lifter = Lifter::new(program, data, walked.instructions)?;
    let mut function = lifter.run(program, code)?;
    // Scalars a compiler parked in lanes of a vector register become
    // variables of their own; what that leaves unread goes next.
    park::unpark(&mut function);
    // Removing one dead write can leave the reads that fed it dead in
    // turn, so this goes round until nothing more is removed.
    while remove_dead(&mut function) != 0 {}
    if make_reducible(&mut function).is_none() {
        return Err(fail(
            "the program",
            "the control flow cannot be made reducible",
        ));
    }
    debug_assert!(function.verify().is_ok(), "{:?}", function.verify());
    Ok(function)
}

/// The registers an instruction writes and reads, as (vector register,
/// written) pairs, for the small analyses that need them.
fn vector_uses(instruction: &Instruction) -> Vec<(u8, bool)> {
    let mut out = Vec::new();
    for arg in &instruction.args {
        if let Operand::Vector { first, count } = arg.operand {
            let written = matches!(arg.role, Role::Result | Role::SecondResult);
            for index in 0..count {
                out.push((first.wrapping_add(index), written));
            }
        }
    }
    out
}

/// Whether an instruction writes the live-lane mask.
fn writes_exec(instruction: &Instruction) -> bool {
    let name = instruction.mnemonic.unwrap_or("");
    name.starts_with("v_cmpx_")
        || name.contains("saveexec")
        || name.contains("wrexec")
        || instruction.args.iter().any(|arg| {
            matches!(arg.role, Role::Result | Role::SecondResult)
                && matches!(arg.operand, Operand::Exec(_))
        })
}

impl<'a> Lifter<'a> {
    /// Finds the blocks of the program and the facts that hold for all of
    /// it.
    fn new(
        program: &Program,
        data: &'a StageData,
        instructions: Vec<Instruction>,
    ) -> Lift<Lifter<'a>> {
        let fail = |instruction: &Instruction, reason| LiftError {
            offset: Some(instruction.offset),
            instruction: instruction.mnemonic.unwrap_or("an unknown instruction"),
            reason,
        };
        // Far jumps: the walk recognised them to get here, and the same
        // recognition names their targets.
        let by_word: BTreeMap<usize, Instruction> = instructions
            .iter()
            .map(|instruction| ((instruction.offset / 4) as usize, instruction.clone()))
            .collect();
        let mut far_jumps = HashMap::new();
        let mut leaders = vec![0u32];
        for (index, instruction) in instructions.iter().enumerate() {
            if instruction.is_unknown() {
                return Err(fail(instruction, "the instruction is not decoded"));
            }
            let mut target = instruction.branch_target();
            if instruction.mnemonic == Some("s_setpc_b64") {
                target = crate::gfx10::far_jump_target(&by_word, instruction);
                match target {
                    Some(target) if target >= 0 => {
                        far_jumps.insert(instruction.offset, target as u32);
                    }
                    _ => return Err(fail(instruction, "a jump through a register")),
                }
            }
            let branches = target.is_some() && instruction.mnemonic != Some("s_call_b64");
            if let (true, Some(target)) = (branches, target) {
                let target = u32::try_from(target)
                    .map_err(|_| fail(instruction, "a branch to before the program"))?;
                leaders.push(target);
            }
            if (branches || instruction.ends_block())
                && let Some(next) = instructions.get(index + 1)
            {
                leaders.push(next.offset);
            }
        }
        leaders.sort_unstable();
        leaders.dedup();
        let index_of: HashMap<u32, usize> = instructions
            .iter()
            .enumerate()
            .map(|(index, instruction)| (instruction.offset, index))
            .collect();
        let mut starts = Vec::new();
        let mut block_at = HashMap::new();
        for leader in leaders {
            // A branch target that is not the start of a decoded instruction
            // was already refused by the walk; skip it here.
            if let Some(&index) = index_of.get(&leader) {
                block_at.insert(leader, starts.len());
                starts.push(index);
            }
        }

        // The registers the weights arrive in.
        let mut weights = Vec::new();
        if let StageData::Pixel(pixel) = data {
            for (input, register) in pixel.system_inputs() {
                let pair = match input {
                    PixelSystemInput::PerspectiveSample => Weights::PerspectiveSample,
                    PixelSystemInput::PerspectiveCenter => Weights::PerspectiveCenter,
                    PixelSystemInput::PerspectiveCentroid => Weights::PerspectiveCentroid,
                    PixelSystemInput::LinearSample => Weights::LinearSample,
                    PixelSystemInput::LinearCenter => Weights::LinearCenter,
                    PixelSystemInput::LinearCentroid => Weights::LinearCentroid,
                    _ => continue,
                };
                weights.push((register, pair, false));
                weights.push((register + 1, pair, true));
            }
        }

        let lifter = Lifter {
            stage: program.stage,
            data,
            wave: program.wave,
            instructions,
            starts,
            block_at,
            far_jumps,
            weights,
            temps: 0,
            insts: Vec::new(),
            state: State::start(),
            at: (None, "the program"),
            remember_export: false,
            paired: HashMap::new(),
        };
        Ok(lifter)
    }

    /// The weight a register holds, if it still holds what the stage
    /// handed over.
    fn weight_in(&self, operand: Option<Operand>) -> Option<(Weights, bool)> {
        let Some(Operand::Vector { first, .. }) = operand else {
            return None;
        };
        self.weights
            .iter()
            .enumerate()
            .find(|&(index, &(register, _, _))| {
                register == first && self.state.pristine >> index & 1 != 0
            })
            .map(|(_, &(_, weights, second))| (weights, second))
    }

    /// For the first half of an interpolation at `index`: the index of the
    /// second half that completes it and the weights, if the two halves use
    /// one pair of weights exactly as the stage handed them over and
    /// nothing in between reads the half-done value, writes the second
    /// weight or changes the live mask. Such a pair reads as one
    /// interpolated input.
    fn interpolation_pair(&self, index: usize) -> Option<(usize, Weights)> {
        let first = &self.instructions[index];
        let (weights, false) = self.weight_in(first.arg(Role::Source))? else {
            return None;
        };
        let Some(Operand::Vector { first: target, .. }) = first.arg(Role::Result) else {
            return None;
        };
        // The weight registers written since the first half.
        let mut written_weights: Vec<u8> = Vec::new();
        for (later, second) in self.instructions.iter().enumerate().skip(index + 1) {
            if self.starts.binary_search(&later).is_ok() {
                return None;
            }
            let same = second.mnemonic == Some("v_interp_p2_f32")
                && second.detail == first.detail
                && second.arg(Role::Result) == first.arg(Role::Result);
            if same {
                let source = second.arg(Role::Source);
                let untouched = !matches!(source, Some(Operand::Vector { first, .. })
                    if first == target || written_weights.contains(&first));
                let complete = untouched && self.weight_in(source) == Some((weights, true));
                return complete.then_some((later, weights));
            }
            // Anything that touches the half-done register or changes the
            // live mask ends the search.
            let uses = vector_uses(second);
            if uses.iter().any(|&(register, _)| register == target)
                || writes_exec(second)
                || second.ends_block()
            {
                return None;
            }
            written_weights.extend(
                uses.iter()
                    .filter(|&&(_, written)| written)
                    .map(|&(register, _)| register),
            );
        }
        None
    }

    /// Fails with the instruction being lifted.
    fn fail<T>(&self, reason: &'static str) -> Lift<T> {
        Err(LiftError {
            offset: self.at.0,
            instruction: self.at.1,
            reason,
        })
    }

    /// Runs the lift: the facts to a fixed point, then the blocks.
    fn run(&mut self, program: &Program, code: Vec<u32>) -> Lift<Function> {
        // The entry block's instructions set up the inputs and give the
        // state the first machine block starts in.
        self.state = State::start();
        self.insts = Vec::new();
        self.at = (None, "the program's inputs");
        self.inputs(program)?;
        let entry_insts = std::mem::take(&mut self.insts);
        let entry_state = self.state.clone();

        // Each machine block's entry state, to a fixed point.
        let wide = self.mask_words() == 2;
        let mut entry: Vec<Option<State>> = vec![None; self.starts.len()];
        entry[0] = Some(entry_state);
        let mut work = vec![0usize];
        let mut rounds = 0usize;
        while let Some(block) = work.pop() {
            rounds += 1;
            if rounds > 64 * self.starts.len() + 1024 {
                self.at = (None, "the program");
                return self.fail("the register forms do not settle");
            }
            let Some(state) = entry[block].clone() else {
                continue;
            };
            let lifted = self.block(block, state)?;
            for (successor, out) in lifted.out {
                let changed = match &mut entry[successor] {
                    Some(known) => known.meet(&out, wide),
                    slot @ None => {
                        *slot = Some(out);
                        true
                    }
                };
                if changed && !work.contains(&successor) {
                    work.push(successor);
                }
            }
        }

        // The blocks, for real. Temporaries handed out so far are dropped.
        self.temps = entry_insts
            .iter()
            .map(|inst| inst.result.0 + u32::from(inst.op.result_count()))
            .max()
            .unwrap_or(0);
        let mut number: Vec<Option<BlockId>> = vec![None; self.starts.len()];
        let mut next = 1;
        for (block, state) in entry.iter().enumerate() {
            if state.is_some() {
                number[block] = Some(next);
                next += 1;
            }
        }
        let target = |number: &[Option<BlockId>], block: usize| number[block].unwrap_or(0);
        let mut blocks = vec![Block {
            offset: None,
            insts: entry_insts,
            terminator: Terminator::Jump(target(&number, 0)),
        }];
        for (block, state) in entry.iter().enumerate() {
            let Some(state) = state.clone() else {
                continue;
            };
            let mut lifted = self.block(block, state)?;
            // What the blocks after this one expect that it does not have.
            self.insts = std::mem::take(&mut lifted.insts);
            for (successor, out) in &lifted.out {
                if let Some(wanted) = &entry[*successor] {
                    self.convert(out, wanted)?;
                }
            }
            lifted.insts = std::mem::take(&mut self.insts);
            let terminator = match lifted.end {
                End::Jump(to) => Terminator::Jump(target(&number, to)),
                End::Branch {
                    condition,
                    if_true,
                    if_false,
                } => Terminator::Branch {
                    condition,
                    if_true: target(&number, if_true),
                    if_false: target(&number, if_false),
                },
                End::Return => Terminator::Return,
            };
            blocks.push(Block {
                offset: Some(self.instructions[self.starts[block]].offset),
                insts: lifted.insts,
                terminator,
            });
        }

        let mut interface = Interface::default();
        match self.data {
            StageData::Pixel(pixel) => {
                interface.depth_control = pixel.depth_control;
                interface.pixel_inputs = pixel
                    .inputs
                    .iter()
                    .map(|input| AttributeRow {
                        attribute: input.attribute,
                        row: input.row,
                        mask: input.mask,
                        flat: input.is_flat(),
                    })
                    .collect();
            }
            StageData::Vertex(vertex) => {
                interface.outputs = vertex
                    .outputs
                    .iter()
                    .map(|output| ParameterRow {
                        parameter: output.parameter,
                        row: output.row,
                    })
                    .collect();
            }
            StageData::Compute(compute) => {
                interface.group_size = compute.group_size;
                interface.shared_bytes = compute.shared_memory_bytes();
            }
            _ => {}
        }
        Ok(Function {
            stage: self.stage,
            wave: self.wave,
            blocks,
            temps: self.temps,
            interface,
            code,
        })
    }

    /// At the end of a block: makes the forms of scalar registers that the
    /// next block expects (`wanted`) and this one does not have (`have`).
    /// A number becomes the lane's own bit of it; a mask becomes the lanes'
    /// bits gathered into a number.
    fn convert(&mut self, have: &State, wanted: &State) -> Lift<()> {
        let wide = self.mask_words() == 2;
        self.at = (None, "the program");
        for index in 0..SCALARS - 1 {
            let register = index as u8;
            let (mine, theirs) = (have.forms[index], wanted.forms[index]);
            let missing = theirs & !mine;
            let flag = |lifter: &mut Self, start: u8| {
                if have.forms[usize::from(start)] & TRUE != 0 {
                    Value::Flag(true)
                } else {
                    lifter.emit(Op::Get(mask_var(start)))
                }
            };
            if missing & MASK != 0 {
                if !(mine & NUM != 0 && (!wide || have.forms[index + 1] & NUM != 0)) {
                    return self.fail("the register forms do not settle");
                }
                let number =
                    |lifter: &mut Self, register: u8| match have.constants[usize::from(register)] {
                        Some(constant) => Value::Word(constant),
                        None => lifter.emit(Op::Get(Var::Scalar(register))),
                    };
                let low = number(self, register);
                let high = if wide {
                    number(self, register + 1)
                } else {
                    Value::Word(0)
                };
                let bit = self.lane_bit(low, high);
                self.emit(Op::Set(mask_var(register), bit));
            }
            if missing & NUM != 0 {
                let (start, high) = if mine & MASK != 0 {
                    (register, false)
                } else if mine & HIGH != 0 && index > 0 {
                    (register - 1, true)
                } else {
                    return self.fail("the register forms do not settle");
                };
                let mask = flag(self, start);
                let word = self.emit(Op::BallotWord { mask, high });
                self.emit(Op::Set(Var::Scalar(register), word));
            }
        }
        Ok(())
    }

    /// Writes the stage's inputs into the registers they arrive in.
    fn inputs(&mut self, program: &Program) -> Lift<()> {
        for parameter in &program.root_parameters {
            for index in 0..parameter.register_count {
                let register = u32::from(parameter.user_data) + index;
                let Ok(register) = u8::try_from(register) else {
                    return self.fail("a root parameter names a register that does not exist");
                };
                if usize::from(register) >= SCALARS - 2 {
                    return self.fail("a root parameter names a register that does not exist");
                }
                let value = self.emit(Op::Input(Input::UserData(register)));
                self.set_number(register, value);
            }
        }
        match self.data {
            StageData::Pixel(pixel) => {
                for (input, register) in pixel.system_inputs() {
                    let weights = |weights| {
                        vec![
                            Input::Weight {
                                weights,
                                second: false,
                            },
                            Input::Weight {
                                weights,
                                second: true,
                            },
                        ]
                    };
                    let inputs = match input {
                        PixelSystemInput::PerspectiveSample => weights(Weights::PerspectiveSample),
                        PixelSystemInput::PerspectiveCenter => weights(Weights::PerspectiveCenter),
                        PixelSystemInput::PerspectiveCentroid => {
                            weights(Weights::PerspectiveCentroid)
                        }
                        PixelSystemInput::LinearSample => weights(Weights::LinearSample),
                        PixelSystemInput::LinearCenter => weights(Weights::LinearCenter),
                        PixelSystemInput::LinearCentroid => weights(Weights::LinearCentroid),
                        PixelSystemInput::PositionX => vec![Input::Position(0)],
                        PixelSystemInput::PositionY => vec![Input::Position(1)],
                        PixelSystemInput::PositionZ => vec![Input::Position(2)],
                        PixelSystemInput::PositionW => vec![Input::Position(3)],
                        PixelSystemInput::FrontFace => vec![Input::FrontFace],
                        PixelSystemInput::Ancillary => vec![Input::Ancillary],
                        PixelSystemInput::SampleCoverage => vec![Input::SampleCoverage],
                        PixelSystemInput::FixedPosition => vec![Input::FixedPosition],
                        PixelSystemInput::PerspectivePullModel | PixelSystemInput::LineStipple => {
                            return self.fail("a pixel input the lift has no rule for");
                        }
                    };
                    for (index, input) in inputs.into_iter().enumerate() {
                        let value = self.emit(Op::Input(input));
                        self.emit(Op::Set(Var::Vector(register + index as u8), value));
                    }
                }
            }
            StageData::Vertex(_) => {
                // Taken on trust from the research note: the vertex number
                // in v0 and the instance number in v3.
                let vertex = self.emit(Op::Input(Input::VertexId));
                self.emit(Op::Set(Var::Vector(0), vertex));
                let instance = self.emit(Op::Input(Input::InstanceId));
                self.emit(Op::Set(Var::Vector(3), instance));
            }
            StageData::Compute(compute) => {
                for axis in 0..compute.thread_id_components() {
                    let value = self.emit(Op::Input(Input::ThreadId(axis)));
                    self.emit(Op::Set(Var::Vector(axis), value));
                }
                let mut register = compute.user_registers();
                for axis in 0..3 {
                    if compute.has_group_id(axis) {
                        let value = self.emit(Op::Input(Input::GroupId(axis)));
                        self.set_number(register, value);
                        register += 1;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Lifts one machine block from the given entry state.
    fn block(&mut self, block: usize, state: State) -> Lift<Lifted> {
        self.state = state;
        // Which registers get both forms on the way in is a fact about the
        // block's entry, not about what leaves it.
        self.state.both = [0; 2];
        self.insts = Vec::new();
        let first = self.starts[block];
        let last = self
            .starts
            .get(block + 1)
            .copied()
            .unwrap_or(self.instructions.len());
        let mut end = None;
        let mut index = first;
        while index < last {
            let instruction = self.instructions[index].clone();
            let name = instruction.mnemonic.unwrap_or("an unknown instruction");
            self.at = (Some(instruction.offset), name);
            if let Some(found) = self.control(&instruction)? {
                end = Some(found);
                break;
            }
            // A wave reduction the compiler wrote out is lifted as a whole.
            if let Some(after) = self.wave_section(index, last)? {
                index = after;
                continue;
            }
            self.at = (Some(instruction.offset), name);
            self.instruction(index, &instruction)?;
            index += 1;
        }
        let end = match end {
            Some(end) => end,
            None => {
                // The block runs into the next one.
                match self.instructions.get(last) {
                    Some(next) => End::Jump(self.block_of(next.offset)?),
                    None => return self.fail("the program runs past its last instruction"),
                }
            }
        };
        let mut out = Vec::new();
        match &end {
            End::Jump(to) => out.push((*to, self.state.clone())),
            End::Branch {
                if_true, if_false, ..
            } => {
                // A branch on the live flag tells each side what it is.
                let on_live = matches!(
                    name_of(&self.instructions[last - 1]),
                    "s_cbranch_execz" | "s_cbranch_execnz"
                );
                let mut taken = self.state.clone();
                let mut other = self.state.clone();
                if on_live {
                    taken.live = true;
                    other.live = false;
                }
                out.push((*if_true, taken));
                out.push((*if_false, other));
            }
            End::Return => {}
        }
        Ok(Lifted {
            insts: std::mem::take(&mut self.insts),
            end,
            out,
        })
    }

    /// The machine block that starts at a byte offset.
    fn block_of(&self, offset: u32) -> Lift<usize> {
        match self.block_at.get(&offset) {
            Some(&block) => Ok(block),
            None => self.fail("a branch into the middle of an instruction"),
        }
    }

    /// Lifts an instruction that ends a block, or returns `None` for any
    /// other.
    fn control(&mut self, instruction: &Instruction) -> Lift<Option<End>> {
        let name = name_of(instruction);
        let next = instruction.end();
        let target = |lifter: &Self| -> Lift<usize> {
            match instruction.branch_target() {
                Some(target) if target >= 0 => lifter.block_of(target as u32),
                _ => lifter.fail("a branch to before the program"),
            }
        };
        // A conditional branch: `condition` true goes to the target when
        // `when` is true, else the fallthrough.
        let conditional = |lifter: &mut Self, condition: Value, when: bool| -> Lift<End> {
            let to = target(lifter)?;
            let fallthrough = lifter.block_of(next)?;
            let (if_true, if_false) = if when {
                (to, fallthrough)
            } else {
                (fallthrough, to)
            };
            Ok(match condition {
                // A flag known here leaves one way to go. A branch to its
                // own fallthrough is no branch either.
                Value::Flag(true) => End::Jump(if_true),
                Value::Flag(false) => End::Jump(if_false),
                _ if if_true == if_false => End::Jump(if_true),
                _ => End::Branch {
                    condition,
                    if_true,
                    if_false,
                },
            })
        };
        Ok(Some(match name {
            "s_branch" => End::Jump(target(self)?),
            "s_cbranch_scc0" | "s_cbranch_scc1" => {
                if self.state.condition == Condition::Unknown {
                    return self.fail("the condition code differs between the paths that get here");
                }
                let condition = self.emit(Op::Get(Var::Scc));
                conditional(self, condition, name == "s_cbranch_scc1")?
            }
            "s_cbranch_vccz" | "s_cbranch_vccnz" => {
                let condition = if self.state.forms[VCC] & MASK != 0 {
                    self.emit(Op::Get(Var::Vcc))
                } else {
                    // A number: the wave goes by whether it is zero.
                    let low = self.word_at(Operand::Vcc(Half::Both), 0)?;
                    let high = if self.mask_words() == 2 {
                        self.word_at(Operand::Vcc(Half::Both), 1)?
                    } else {
                        Value::Word(0)
                    };
                    let either = self.binary(BinaryOp::Or, low, high);
                    self.compare(CompareOp::NotEqual, either, Value::Word(0))
                };
                conditional(self, condition, name == "s_cbranch_vccnz")?
            }
            "s_cbranch_execz" | "s_cbranch_execnz" => {
                let condition = self.live();
                let end = conditional(self, condition, name == "s_cbranch_execnz")?;
                if !matches!(end, End::Branch { .. }) {
                    // The successor keeps the state as it is: `block` only
                    // refines it for a real branch.
                }
                end
            }
            "s_endpgm" => {
                if self.stage == HardwareStage::Pixel {
                    // A lane that ends without its final export was killed.
                    match self.state.exported {
                        Exported::Yes => {}
                        Exported::No => {
                            self.emit(Op::Discard(Value::Flag(true)));
                        }
                        Exported::Mixed => {
                            self.remember_export = true;
                            let exported = self.emit(Op::Get(Var::Exported));
                            let missing = self.emit(Op::FlagNot(exported));
                            self.emit(Op::Discard(missing));
                        }
                    }
                }
                End::Return
            }
            "s_setpc_b64" => match self.far_jumps.get(&instruction.offset) {
                Some(&target) => End::Jump(self.block_of(target)?),
                None => return self.fail("a jump through a register"),
            },
            _ if instruction.ends_block() => {
                return self.fail("an end of the program the lift has no rule for");
            }
            _ => return Ok(None),
        }))
    }

    /// Lifts one instruction that does not end a block.
    fn instruction(&mut self, index: usize, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        let result = self.dispatch(index, instruction, name);
        self.forget_weights(instruction);
        result
    }

    /// Notes that the weight registers an instruction writes no longer
    /// hold their weights.
    fn forget_weights(&mut self, instruction: &Instruction) {
        for (register, written) in vector_uses(instruction) {
            if !written {
                continue;
            }
            for (bit, &(weight, _, _)) in self.weights.iter().enumerate() {
                if weight == register {
                    self.state.pristine &= !(1 << bit);
                }
            }
        }
    }

    /// Lifts one instruction by its format.
    fn dispatch(
        &mut self,
        index: usize,
        instruction: &Instruction,
        name: &'static str,
    ) -> Lift<()> {
        match instruction.format {
            Format::Sopp => match name {
                // Scheduling, waiting and hints change nothing a lane sees.
                "s_nop" | "s_waitcnt" | "s_inst_prefetch" | "s_clause" | "s_code_end"
                | "s_waitcnt_depctr" | "s_round_mode" | "s_denorm_mode" | "s_ttracedata"
                | "s_icache_inv" => Ok(()),
                "s_barrier" => {
                    self.emit(Op::Barrier);
                    Ok(())
                }
                _ => self.fail("a program-control instruction the lift has no rule for"),
            },
            Format::Sop2 | Format::Sop1 | Format::Sopc | Format::Sopk => self.scalar(instruction),
            Format::Vop1
            | Format::Vop2
            | Format::Vop3a
            | Format::Vop3b
            | Format::Vopc
            | Format::Vop3p
            | Format::Vintrp => self.vector(index, instruction),
            Format::Smem | Format::Mubuf | Format::Mtbuf | Format::Flat | Format::Global => {
                self.memory(instruction)
            }
            Format::Scratch => self.fail("scratch memory is not lifted"),
            Format::Ds => self.shared(instruction),
            Format::Mimg => self.image(instruction),
            Format::Exp => self.export(instruction),
            Format::Unknown => self.fail("the instruction is not decoded"),
        }
    }

    // ---- Emitting -------------------------------------------------------

    /// Appends an operation and returns its first result. Operations on
    /// constants are computed here instead, so that constants a program
    /// builds (descriptor words, addresses) stay visible as constants.
    fn emit(&mut self, op: Op) -> Value {
        if let Some(value) = fold(&op) {
            return value;
        }
        let result = Temp(self.temps);
        self.temps += u32::from(op.result_count());
        self.insts.push(Inst { result, op });
        Value::Temp(result)
    }

    /// Appends an operation with several results and returns them.
    fn emit_many(&mut self, op: Op) -> Vec<Value> {
        let count = op.result_count();
        let result = Temp(self.temps);
        self.temps += u32::from(count);
        self.insts.push(Inst { result, op });
        (0..u32::from(count))
            .map(|index| Value::Temp(Temp(result.0 + index)))
            .collect()
    }

    fn unary(&mut self, op: UnaryOp, a: Value) -> Value {
        self.emit(Op::Unary(op, a))
    }

    fn binary(&mut self, op: BinaryOp, a: Value, b: Value) -> Value {
        self.emit(Op::Binary(op, a, b))
    }

    fn compare(&mut self, op: CompareOp, a: Value, b: Value) -> Value {
        self.emit(Op::Compare(op, a, b))
    }

    fn select(&mut self, condition: Value, if_true: Value, if_false: Value) -> Value {
        self.emit(Op::Select {
            condition,
            if_true,
            if_false,
        })
    }

    fn flag(&mut self, op: FlagOp, a: Value, b: Value) -> Value {
        self.emit(Op::Flag(op, a, b))
    }

    fn not(&mut self, a: Value) -> Value {
        self.emit(Op::FlagNot(a))
    }

    /// A flag as the number 1 or 0.
    fn number_of_flag(&mut self, flag: Value) -> Value {
        self.select(flag, Value::Word(1), Value::Word(0))
    }

    // ---- Registers ------------------------------------------------------

    /// How many scalar registers a lane mask occupies.
    fn mask_words(&self) -> u8 {
        self.wave.mask_words()
    }

    /// The lane's live flag.
    fn live(&mut self) -> Value {
        if self.state.live {
            Value::Flag(true)
        } else {
            self.emit(Op::Get(Var::Exec))
        }
    }

    /// Checks that a scalar register number is one the state follows.
    fn check_scalar(&self, register: u8) -> Lift<usize> {
        if usize::from(register) < SCALARS - 2 {
            Ok(usize::from(register))
        } else {
            self.fail("a scalar register that does not exist")
        }
    }

    /// Reads a scalar register as a number.
    fn number(&mut self, register: u8) -> Lift<Value> {
        let index = self.check_scalar(register)?;
        if let Some(constant) = self.state.constants[index] {
            return Ok(Value::Word(constant));
        }
        let forms = self.state.forms[index];
        if forms & NUM != 0 {
            Ok(self.emit(Op::Get(Var::Scalar(register))))
        } else if forms & MASK != 0 {
            // A mask read as a number: the lanes' bits, gathered.
            let mask = self.mask_of_register(register);
            Ok(self.emit(Op::BallotWord { mask, high: false }))
        } else if forms & HIGH != 0 {
            let mask = self.mask_of_register(register - 1);
            Ok(self.emit(Op::BallotWord { mask, high: true }))
        } else if forms & LEFT != 0 {
            self.fail("a read of what a whole-wave section left in a register")
        } else {
            self.fail("a scalar register holds a lane mask on one path and a number on another")
        }
    }

    /// The flag of the mask that starts at a register known to hold one.
    fn mask_of_register(&mut self, register: u8) -> Value {
        if self.state.forms[usize::from(register)] & TRUE != 0 {
            Value::Flag(true)
        } else {
            self.emit(Op::Get(Var::Mask(register)))
        }
    }

    /// Forgets what a write to `register` destroys: a 64-lane mask of which
    /// the register is one half.
    fn clobber(&mut self, register: u8) {
        let index = usize::from(register);
        let forms = self.state.forms[index];
        if forms & HIGH != 0 && index > 0 {
            self.state.forms[index - 1] &= !(MASK | TRUE);
        }
        if forms & MASK != 0 && self.mask_words() == 2 {
            self.state.forms[index + 1] &= !HIGH;
        }
    }

    /// Writes a number to a scalar register.
    fn set_number(&mut self, register: u8, value: Value) {
        self.clobber(register);
        self.emit(Op::Set(Var::Scalar(register), value));
        let index = usize::from(register);
        self.state.forms[index] = NUM;
        self.state.constants[index] = value.as_word();
    }

    /// Writes a lane mask, as the lane's flag, to the scalar registers that
    /// start at `register`.
    fn set_mask_register(&mut self, register: u8, flag: Value) {
        let index = usize::from(register);
        self.clobber(register);
        if self.mask_words() == 2 {
            self.clobber(register + 1);
            self.state.forms[index + 1] = HIGH;
            self.state.constants[index + 1] = None;
        }
        self.emit(Op::Set(Var::Mask(register), flag));
        self.state.forms[index] = MASK | if flag == Value::Flag(true) { TRUE } else { 0 };
        self.state.constants[index] = None;
    }

    /// Writes a lane mask to an operand: `EXEC`, `VCC` or scalar registers.
    fn set_mask(&mut self, target: Operand, flag: Value) -> Lift<()> {
        let whole = if self.mask_words() == 2 {
            Half::Both
        } else {
            Half::Low
        };
        match target {
            Operand::Exec(half) if half == whole => {
                self.emit(Op::Set(Var::Exec, flag));
                self.state.live = flag == Value::Flag(true);
            }
            Operand::Vcc(half) if half == whole => {
                self.emit(Op::Set(Var::Vcc, flag));
                self.state.forms[VCC] = MASK;
                self.state.forms[VCC + 1] = HIGH;
                self.state.constants[VCC] = None;
                self.state.constants[VCC + 1] = None;
            }
            Operand::Scalar { first, count } if count == self.mask_words() => {
                self.check_scalar(first)?;
                self.set_mask_register(first, flag);
            }
            Operand::Null => {}
            _ => return self.fail("a lane mask is written to half a mask register"),
        }
        Ok(())
    }

    /// Whether an operand of a mask-wide operation is a lane mask and
    /// cannot be read as a number without gathering lanes.
    fn only_mask(&self, operand: Operand) -> bool {
        match operand {
            Operand::Exec(_) => true,
            Operand::Vcc(half) => {
                let at = VCC + usize::from(half == Half::High);
                self.state.forms[at] & NUM == 0
            }
            Operand::Scalar { first, count } => (0..count).any(|index| {
                let at = usize::from(first) + usize::from(index);
                at < SCALARS
                    && self.state.forms[at] & NUM == 0
                    && self.state.constants[at].is_none()
            }),
            _ => false,
        }
    }

    /// Reads an operand of the width of a lane mask as the lane's flag.
    fn mask(&mut self, operand: Operand) -> Lift<Value> {
        let whole = if self.mask_words() == 2 {
            Half::Both
        } else {
            Half::Low
        };
        match operand {
            Operand::Exec(half) if half == whole => Ok(self.live()),
            Operand::Vcc(half) if half == whole => {
                if self.state.forms[VCC] & MASK != 0 {
                    return Ok(self.emit(Op::Get(Var::Vcc)));
                }
                // A number kept in `VCC`, used as a mask: the lane's own
                // bit of it.
                let low = self.word_at(operand, 0)?;
                let high = if self.mask_words() == 2 {
                    self.word_at(operand, 1)?
                } else {
                    Value::Word(0)
                };
                Ok(self.lane_bit(low, high))
            }
            Operand::Scalar { first, count } if count == self.mask_words() => {
                let index = self.check_scalar(first)?;
                if self.state.forms[index] & MASK != 0 {
                    return Ok(self.mask_of_register(first));
                }
                // A number used as a mask: the lane's own bit of it.
                let low = self.number(first)?;
                let high = if count == 2 {
                    self.number(first + 1)?
                } else {
                    Value::Word(0)
                };
                Ok(self.lane_bit(low, high))
            }
            Operand::Integer(value) => {
                let word = value as i32 as u32;
                Ok(self.lane_bit(Value::Word(word), Value::Word(word)))
            }
            Operand::Literal(word) => Ok(self.lane_bit(Value::Word(word), Value::Word(0))),
            Operand::Null => Ok(Value::Flag(false)),
            _ => self.fail("an operand that is not a whole lane mask is read as one"),
        }
    }

    /// The lane's bit of a 64-bit number. All zeroes and all ones need no
    /// lane number.
    fn lane_bit(&mut self, low: Value, high: Value) -> Value {
        let wide = self.mask_words() == 2;
        match (low, high) {
            (Value::Word(0), Value::Word(0)) => Value::Flag(false),
            (Value::Word(0), _) if !wide => Value::Flag(false),
            (Value::Word(u32::MAX), Value::Word(u32::MAX)) => Value::Flag(true),
            (Value::Word(u32::MAX), _) if !wide => Value::Flag(true),
            _ => self.emit(Op::LaneBit {
                low,
                high: if wide { high } else { Value::Word(0) },
            }),
        }
    }

    /// Reads one 32-bit register or constant operand as a number.
    fn word(&mut self, operand: Operand) -> Lift<Value> {
        self.word_at(operand, 0)
    }

    /// Reads register `index` of an operand as a number. For a constant,
    /// index 1 is the upper half of its 64-bit value.
    fn word_at(&mut self, operand: Operand, index: u8) -> Lift<Value> {
        match operand {
            Operand::Scalar { first, count } if index < count => self.number(first + index),
            Operand::Vector { first, count } if index < count => {
                self.vector_word(first.wrapping_add(index))
            }
            Operand::Integer(value) => Ok(Value::Word(if index == 0 {
                value as i32 as u32
            } else {
                // A 64-bit inline constant is sign-extended.
                (value as i32 >> 31) as u32
            })),
            Operand::Float(value) if index == 0 => Ok(Value::float(value.value())),
            Operand::Literal(word) if index == 0 => Ok(Value::Word(word)),
            // A 32-bit literal in a 64-bit bit operand is zero-extended.
            Operand::Literal(_) => Ok(Value::Word(0)),
            Operand::Null => Ok(Value::Word(0)),
            Operand::M0 if index == 0 => Ok(self.emit(Op::Get(Var::M0))),
            Operand::Scc if index == 0 => {
                let flag = self.uniform_condition()?;
                Ok(self.number_of_flag(flag))
            }
            Operand::Exec(half) | Operand::Vcc(half) => {
                let is_exec = matches!(operand, Operand::Exec(_));
                let high = match (half, index) {
                    (Half::Low, 0) | (Half::Both, 0) => false,
                    (Half::High, 0) | (Half::Both, 1) => true,
                    _ => return self.fail("an operand is read past its width"),
                };
                if high && self.mask_words() == 1 && is_exec {
                    return Ok(Value::Word(0));
                }
                if !is_exec {
                    // A number a program keeps in `VCC`.
                    let at = VCC + usize::from(high);
                    let forms = self.state.forms[at];
                    if let Some(constant) = self.state.constants[at] {
                        return Ok(Value::Word(constant));
                    }
                    if forms & NUM != 0 {
                        return Ok(self.emit(Op::Get(Var::Scalar(at as u8))));
                    }
                    let whole = if high { forms & HIGH } else { forms & MASK };
                    if whole == 0 {
                        return self.fail(
                            "a scalar register holds a lane mask on one path and a number on another",
                        );
                    }
                    if high && self.mask_words() == 1 {
                        return Ok(Value::Word(0));
                    }
                }
                let mask = if is_exec {
                    self.live()
                } else {
                    self.emit(Op::Get(Var::Vcc))
                };
                Ok(self.emit(Op::BallotWord { mask, high }))
            }
            _ => self.fail("an operand the lift has no rule for"),
        }
    }

    /// The condition code as a flag that is the same in every lane.
    fn uniform_condition(&mut self) -> Lift<Value> {
        match self.state.condition {
            Condition::Uniform => Ok(self.emit(Op::Get(Var::Scc))),
            Condition::LaneBit => {
                let bit = self.emit(Op::Get(Var::Scc));
                Ok(self.emit(Op::AnyLane(bit)))
            }
            Condition::Unknown => {
                self.fail("the condition code differs between the paths that get here")
            }
        }
    }

    /// Sets the condition code to a flag every lane shares.
    fn set_condition(&mut self, flag: Value) {
        self.emit(Op::Set(Var::Scc, flag));
        self.state.condition = Condition::Uniform;
    }

    /// Sets the condition code after an operation on masks to the lane's
    /// bit of the result.
    fn set_condition_from_mask(&mut self, flag: Value) {
        self.emit(Op::Set(Var::Scc, flag));
        self.state.condition = if flag.as_flag().is_some() {
            Condition::Uniform
        } else {
            Condition::LaneBit
        };
    }

    /// Writes a word to a vector register: in a lane that is not live the
    /// register keeps its value.
    fn set_vector(&mut self, register: u8, value: Value) {
        let live = self.live();
        self.set_vector_if(register, value, live);
    }

    /// Reads a vector register. One that holds what a whole-wave section
    /// left in it has no value the lift knows.
    fn vector_word(&mut self, register: u8) -> Lift<Value> {
        if self.state.left[usize::from(register / 64)] >> (register % 64) & 1 != 0 {
            return self.fail("a read of what a whole-wave section left in a register");
        }
        Ok(self.emit(Op::Get(Var::Vector(register))))
    }

    /// Writes a word to a vector register in the lanes where `condition`
    /// holds.
    ///
    /// A write makes a register that held the leftover of a whole-wave
    /// section an ordinary one again, also where the write leaves out the
    /// lanes that are not live: compiled code does not read a lane it has
    /// not written, which is the ground the lane branches stand on too.
    fn set_vector_if(&mut self, register: u8, value: Value, condition: Value) {
        self.state.left[usize::from(register / 64)] &= !(1 << (register % 64));
        let value = match condition {
            Value::Flag(true) => value,
            _ => {
                let old = self.emit(Op::Get(Var::Vector(register)));
                self.select(condition, value, old)
            }
        };
        self.emit(Op::Set(Var::Vector(register), value));
    }

    /// The first vector register of an operand.
    fn vector_register(&self, operand: Option<Operand>) -> Lift<(u8, u8)> {
        match operand {
            Some(Operand::Vector { first, count }) => Ok((first, count)),
            _ => self.fail("an operand that should be a vector register is not"),
        }
    }

    /// The first scalar register of an operand.
    fn scalar_register(&self, operand: Option<Operand>) -> Lift<(u8, u8)> {
        match operand {
            Some(Operand::Scalar { first, count }) => {
                self.check_scalar(first.saturating_add(count.saturating_sub(1)))?;
                Ok((first, count))
            }
            _ => self.fail("an operand that should be scalar registers is not"),
        }
    }

    // ---- Exports --------------------------------------------------------

    /// Lifts an export.
    fn export(&mut self, instruction: &Instruction) -> Lift<()> {
        let Detail::Export {
            target,
            enabled,
            compressed,
            done,
            ..
        } = instruction.detail
        else {
            return self.fail("the instruction is not decoded");
        };
        if compressed {
            return self.fail("an export of packed 16-bit values");
        }
        let pixel = self.stage == HardwareStage::Pixel;
        let target = match target {
            0..=7 if pixel => Some(ExportTarget::Color(target)),
            8 if pixel => Some(ExportTarget::Depth),
            9 => None,
            12..=16 if !pixel => Some(ExportTarget::Position(target - 12)),
            32..=63 if !pixel => Some(ExportTarget::Parameter(target - 32)),
            _ => return self.fail("an export target the stage does not have"),
        };
        let mut values = [None; 4];
        let mut sources = instruction.sources();
        for (channel, value) in values.iter_mut().enumerate() {
            if enabled >> channel & 1 != 0 {
                let Some(source) = sources.next() else {
                    return self.fail("the instruction is not decoded");
                };
                *value = Some(self.word(source)?);
            }
        }
        let guard = self.live();
        if let Some(target) = target {
            self.emit(Op::Export {
                target,
                values,
                guard,
            });
        }
        if pixel && done {
            // There is no discard instruction: a lane that is not live at
            // the final export is discarded.
            let dead = self.not(guard);
            if dead != Value::Flag(false) {
                self.emit(Op::Discard(dead));
            }
            if self.remember_export {
                self.emit(Op::Set(Var::Exported, Value::Flag(true)));
            }
            self.state.exported = Exported::Yes;
        }
        Ok(())
    }
}

/// The mnemonic of a decoded instruction.
fn name_of(instruction: &Instruction) -> &'static str {
    instruction.mnemonic.unwrap_or("")
}

/// Computes an operation whose operands are all constants, and simplifies
/// the flag operations with one constant operand.
fn fold(op: &Op) -> Option<Value> {
    match *op {
        Op::Unary(kind, Value::Word(a)) => Some(Value::Word(eval::unary(kind, a))),
        Op::Binary(kind, Value::Word(a), Value::Word(b)) => {
            Some(Value::Word(eval::binary(kind, a, b)))
        }
        // Operations that leave a word as it is, or give zero whatever it
        // is. The lift produces many: an offset of zero, a mask of all
        // ones.
        Op::Binary(BinaryOp::Add | BinaryOp::Or | BinaryOp::Xor, Value::Word(0), other)
        | Op::Binary(
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Or
            | BinaryOp::Xor
            | BinaryOp::ShiftLeft
            | BinaryOp::ShiftRight
            | BinaryOp::ShiftRightSigned,
            other,
            Value::Word(0),
        )
        | Op::Binary(BinaryOp::And, Value::Word(u32::MAX), other)
        | Op::Binary(BinaryOp::And, other, Value::Word(u32::MAX)) => Some(other),
        Op::Binary(BinaryOp::And, Value::Word(0), _)
        | Op::Binary(BinaryOp::And, _, Value::Word(0)) => Some(Value::Word(0)),
        Op::Carry {
            b: Value::Word(0),
            carry_in: Value::Flag(false),
            ..
        }
        | Op::Carry {
            a: Value::Word(0),
            carry_in: Value::Flag(false),
            ..
        }
        | Op::Borrow {
            b: Value::Word(0),
            borrow_in: Value::Flag(false),
            ..
        } => Some(Value::Flag(false)),
        Op::Compare(kind, Value::Word(a), Value::Word(b)) => {
            Some(Value::Flag(eval::compare(kind, a, b)))
        }
        Op::FlagNot(Value::Flag(a)) => Some(Value::Flag(!a)),
        Op::Flag(kind, a, b) => {
            match (kind, a, b) {
                (FlagOp::And, Value::Flag(false), _) | (FlagOp::And, _, Value::Flag(false)) => {
                    Some(Value::Flag(false))
                }
                (FlagOp::And, Value::Flag(true), other)
                | (FlagOp::And, other, Value::Flag(true)) => Some(other),
                (FlagOp::Or, Value::Flag(true), _) | (FlagOp::Or, _, Value::Flag(true)) => {
                    Some(Value::Flag(true))
                }
                (FlagOp::Or, Value::Flag(false), other)
                | (FlagOp::Or, other, Value::Flag(false)) => Some(other),
                (FlagOp::Xor, Value::Flag(false), other)
                | (FlagOp::Xor, other, Value::Flag(false)) => Some(other),
                (FlagOp::Xor, Value::Flag(x), Value::Flag(y)) => Some(Value::Flag(x ^ y)),
                _ => None,
            }
        }
        Op::Select {
            condition: Value::Flag(condition),
            if_true,
            if_false,
        } => Some(if condition { if_true } else { if_false }),
        Op::Select {
            if_true, if_false, ..
        } if if_true == if_false => Some(if_true),
        Op::Carry {
            a: Value::Word(a),
            b: Value::Word(b),
            carry_in: Value::Flag(carry_in),
        } => Some(Value::Flag(eval::carry(a, b, carry_in))),
        Op::Borrow {
            a: Value::Word(a),
            b: Value::Word(b),
            borrow_in: Value::Flag(borrow_in),
        } => Some(Value::Flag(eval::borrow(a, b, borrow_in))),
        // A flag every lane holds alike is its own "any lane", since the
        // lane asking is one of them.
        Op::QuadAny(Value::Flag(flag)) | Op::AnyLane(Value::Flag(flag)) => Some(Value::Flag(flag)),
        _ => None,
    }
}

/// Analyses a lifted function's control flow. A convenience for callers
/// that hold only this module.
pub fn control_flow(function: &Function) -> ControlFlow {
    ControlFlow::of(function)
}
