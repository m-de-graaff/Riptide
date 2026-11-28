//! A direct interpreter of the machine instructions, a whole wave at a
//! time.
//!
//! This is the second opinion the lift is checked against. It knows nothing
//! of the lift: registers are plain 32-bit words, lane masks are plain
//! numbers with a bit per lane, `EXEC` gates vector writes, and branches
//! test the whole wave, all as AMD's public description of the instruction
//! set states. A test runs a synthetic program here and through
//! [`crate::lift`] and [`super::run_wave`], with the same [`Host`], and
//! compares what was exported, stored and discarded.
//!
//! It covers the instructions the tests use, which are the common ones the
//! lift covers; an instruction it has no rule for gives a [`MachineError`].
//! The arithmetic of the simple operations is written out here a second
//! time. The float rules that take care to get right (minimum and maximum,
//! the median, conversions to 16-bit floats, sine and cosine in turns, the
//! byte permutation, cube faces) and the buffer descriptor are shared with
//! the reference interpreter through [`crate::ir::eval`], and have their
//! own tests there.
//!
//! Lane permutations, atomic operations and the group-shared memory are
//! done here as the instruction set describes them, a wave at a time: every
//! lane reads before any lane writes, and where lanes change memory they do
//! so in the order of their numbers. A wave is taken to be the whole thread
//! group, so a barrier is nothing to wait for.

use std::collections::{BTreeMap, HashMap};

use super::Host;
use crate::container::stage::{ExportFormat, PixelSystemInput, StageData};
use crate::container::xbox::{HardwareStage, Program};
use crate::gfx10::{
    Detail, Format, Half, Instruction, LanePermutation, Operand, Role, VectorModifiers, WaveSize,
    far_jump_target, walk,
};
use crate::ir::eval::{self, BufferFormat, BufferView};
use crate::ir::{
    BinaryOp, Dimension, ExportTarget, ImageKind, ImageOp, Input, Level, Sampler, TernaryOp,
    UnaryOp, Value, VertexTerm, Weights,
};

/// Why the machine interpreter stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineError {
    /// The byte offset of the instruction.
    pub offset: u32,
    /// Its mnemonic.
    pub instruction: &'static str,
    /// What is wrong.
    pub reason: &'static str,
}

impl std::fmt::Display for MachineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at {:#x}: {}",
            self.instruction, self.offset, self.reason
        )
    }
}

impl std::error::Error for MachineError {}

/// How many instructions a run may execute.
const STEP_LIMIT: u64 = 10_000_000;

/// The scalar register codes of the special registers.
const VCC: usize = 106;
const M0: usize = 124;
const EXEC: usize = 126;

/// The state of one wave.
struct Wave {
    /// The vector registers: 256 of them, a word per lane.
    v: Vec<[u32; 64]>,
    /// The scalar registers by operand code, so that `VCC`, `M0` and `EXEC`
    /// sit among them.
    s: [u32; 128],
    /// The scalar condition code.
    scc: bool,
    /// Whether a lane mask is 64 bits.
    wide: bool,
    /// The lanes that exist.
    started: u64,
    /// Whether the final export of a pixel program was passed.
    exported: bool,
    /// Whether conversions to 16-bit floats round toward zero.
    half_toward_zero: bool,
    /// The group-shared memory of a compute program, zeroes at the start.
    shared: Vec<u8>,
}

/// Where the program's own code appears in memory: `s_getpc_b64` gives
/// addresses from here, and a scalar load from this range reads code words.
const CODE_BASE: u64 = 0x7FFF_0000_0000;

impl Wave {
    fn mask(&self, code: usize) -> u64 {
        if self.wide {
            u64::from(self.s[code]) | u64::from(self.s[code + 1]) << 32
        } else {
            u64::from(self.s[code])
        }
    }

    fn set_mask(&mut self, code: usize, value: u64) {
        self.s[code] = value as u32;
        if self.wide {
            self.s[code + 1] = (value >> 32) as u32;
        }
    }

    fn exec(&self) -> u64 {
        self.mask(EXEC)
    }
}

/// The scalar register code an operand starts at.
fn code_of(operand: Operand) -> Option<usize> {
    Some(match operand {
        Operand::Scalar { first, .. } => usize::from(first),
        Operand::Vcc(Half::Low | Half::Both) => VCC,
        Operand::Vcc(Half::High) => VCC + 1,
        Operand::Exec(Half::Low | Half::Both) => EXEC,
        Operand::Exec(Half::High) => EXEC + 1,
        Operand::M0 => M0,
        _ => return None,
    })
}

/// Runs a program on lanes 0 to `lanes - 1` of one wave.
pub fn run_machine(
    program: &Program,
    data: &StageData,
    lanes: usize,
    host: &mut dyn Host,
) -> Result<(), MachineError> {
    let code = match data {
        StageData::Pixel(pixel) => pixel
            .patched_code(&program.code, ExportFormat::Abgr32)
            .unwrap_or_else(|_| program.code.clone()),
        _ => program.code.clone(),
    };
    let instructions = walk(&code, program.wave).instructions;
    let by_word: BTreeMap<usize, Instruction> = instructions
        .iter()
        .map(|instruction| ((instruction.offset / 4) as usize, instruction.clone()))
        .collect();
    let index_of: HashMap<u32, usize> = instructions
        .iter()
        .enumerate()
        .map(|(index, instruction)| (instruction.offset, index))
        .collect();
    let wide = program.wave == WaveSize::Wave64;
    let started = if lanes >= 64 {
        u64::MAX
    } else {
        (1u64 << lanes) - 1
    };
    let mut wave = Wave {
        v: vec![[0; 64]; 256],
        s: [0; 128],
        scc: false,
        wide,
        started,
        exported: false,
        half_toward_zero: false,
        shared: match data {
            StageData::Compute(compute) => vec![0; compute.shared_memory_bytes() as usize],
            _ => Vec::new(),
        },
    };
    wave.set_mask(EXEC, started);
    let mut machine = Machine {
        wave,
        data,
        stage: program.stage,
        code: &code,
        host,
        lanes,
        at: (0, ""),
    };
    machine.inputs(program);

    let mut index = 0usize;
    let mut steps = 0u64;
    loop {
        steps += 1;
        let Some(instruction) = instructions.get(index) else {
            return machine.fail("the program runs past its last instruction");
        };
        machine.at = (instruction.offset, instruction.mnemonic.unwrap_or("?"));
        if steps > STEP_LIMIT {
            return machine.fail("the step limit was reached");
        }
        let goes = |offset: i64| -> Option<usize> {
            u32::try_from(offset)
                .ok()
                .and_then(|offset| index_of.get(&offset).copied())
        };
        match machine.step(instruction)? {
            Flow::Next => index += 1,
            Flow::End => return Ok(()),
            Flow::Branch => {
                let target = instruction.branch_target().and_then(goes);
                match target {
                    Some(target) => index = target,
                    None => return machine.fail("a branch to nowhere"),
                }
            }
            Flow::FarJump => match far_jump_target(&by_word, instruction).and_then(goes) {
                Some(target) => index = target,
                None => return machine.fail("a jump through a register"),
            },
        }
    }
}

/// Where execution goes after an instruction.
enum Flow {
    Next,
    Branch,
    FarJump,
    End,
}

/// A wave being run.
struct Machine<'a> {
    wave: Wave,
    data: &'a StageData,
    stage: HardwareStage,
    code: &'a [u32],
    host: &'a mut dyn Host,
    lanes: usize,
    at: (u32, &'static str),
}

type Run<T> = Result<T, MachineError>;

impl Machine<'_> {
    fn fail<T>(&self, reason: &'static str) -> Run<T> {
        Err(MachineError {
            offset: self.at.0,
            instruction: self.at.1,
            reason,
        })
    }

    /// The lanes that are live, lowest first. A program can make lanes
    /// live that never started (a wave reduction runs in every lane of the
    /// wave), so this goes by the live mask alone.
    fn live_lanes(&self) -> Vec<usize> {
        let exec = self.wave.exec();
        (0..32 * self.mask_words())
            .filter(|&lane| exec >> lane & 1 != 0)
            .collect()
    }

    /// Writes the stage's inputs into the registers.
    fn inputs(&mut self, program: &Program) {
        for parameter in &program.root_parameters {
            for index in 0..parameter.register_count {
                let register = (u32::from(parameter.user_data) + index) as usize;
                if register < 106 {
                    self.wave.s[register] = self.host.input(0, Input::UserData(register as u8));
                }
            }
        }
        match self.data {
            StageData::Pixel(pixel) => {
                for (input, register) in pixel.system_inputs() {
                    let pair = |weights| {
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
                        PixelSystemInput::PerspectiveSample => pair(Weights::PerspectiveSample),
                        PixelSystemInput::PerspectiveCenter => pair(Weights::PerspectiveCenter),
                        PixelSystemInput::PerspectiveCentroid => pair(Weights::PerspectiveCentroid),
                        PixelSystemInput::LinearSample => pair(Weights::LinearSample),
                        PixelSystemInput::LinearCenter => pair(Weights::LinearCenter),
                        PixelSystemInput::LinearCentroid => pair(Weights::LinearCentroid),
                        PixelSystemInput::PositionX => vec![Input::Position(0)],
                        PixelSystemInput::PositionY => vec![Input::Position(1)],
                        PixelSystemInput::PositionZ => vec![Input::Position(2)],
                        PixelSystemInput::PositionW => vec![Input::Position(3)],
                        PixelSystemInput::FrontFace => vec![Input::FrontFace],
                        PixelSystemInput::Ancillary => vec![Input::Ancillary],
                        PixelSystemInput::SampleCoverage => vec![Input::SampleCoverage],
                        PixelSystemInput::FixedPosition => vec![Input::FixedPosition],
                        _ => vec![],
                    };
                    for (index, input) in inputs.into_iter().enumerate() {
                        for lane in 0..self.lanes {
                            self.wave.v[usize::from(register) + index][lane] =
                                self.host.input(lane, input);
                        }
                    }
                }
            }
            StageData::Vertex(_) => {
                for lane in 0..self.lanes {
                    self.wave.v[0][lane] = self.host.input(lane, Input::VertexId);
                    self.wave.v[3][lane] = self.host.input(lane, Input::InstanceId);
                }
            }
            StageData::Compute(compute) => {
                for axis in 0..compute.thread_id_components() {
                    for lane in 0..self.lanes {
                        self.wave.v[usize::from(axis)][lane] =
                            self.host.input(lane, Input::ThreadId(axis));
                    }
                }
                let mut register = usize::from(compute.user_registers());
                for axis in 0..3 {
                    if compute.has_group_id(axis) {
                        self.wave.s[register] = self.host.input(0, Input::GroupId(axis));
                        register += 1;
                    }
                }
            }
            _ => {}
        }
    }

    /// Reads word `index` of a scalar operand or constant.
    fn read(&self, operand: Operand, index: usize) -> Run<u32> {
        if let Some(code) = code_of(operand) {
            return Ok(self.wave.s[code + index]);
        }
        Ok(match operand {
            Operand::Integer(value) if index == 0 => value as i32 as u32,
            Operand::Integer(value) => (i32::from(value) >> 31) as u32,
            Operand::Float(value) if index == 0 => value.value().to_bits(),
            Operand::Literal(word) if index == 0 => word,
            Operand::Literal(_) | Operand::Null => 0,
            Operand::Scc => u32::from(self.wave.scc),
            _ => return self.fail("an operand the machine has no rule for"),
        })
    }

    /// Reads a scalar operand of `width` registers as one number.
    fn read_wide(&self, operand: Operand, width: usize) -> Run<u64> {
        let low = u64::from(self.read(operand, 0)?);
        if width == 2 {
            Ok(low | u64::from(self.read(operand, 1)?) << 32)
        } else {
            Ok(low)
        }
    }

    /// Writes a number to a scalar operand of `width` registers.
    fn write_wide(&mut self, operand: Option<Operand>, width: usize, value: u64) -> Run<()> {
        match operand {
            Some(Operand::Null) => Ok(()),
            Some(operand) => match code_of(operand) {
                Some(code) => {
                    self.wave.s[code] = value as u32;
                    if width == 2 {
                        self.wave.s[code + 1] = (value >> 32) as u32;
                    }
                    Ok(())
                }
                None => self.fail("a result operand the machine has no rule for"),
            },
            None => self.fail("an instruction without the result it should have"),
        }
    }

    /// Reads an operand as one lane sees it.
    fn lane_word(&self, operand: Operand, lane: usize) -> Run<u32> {
        match operand {
            Operand::Vector { first, .. } => Ok(self.wave.v[usize::from(first)][lane]),
            other => self.read(other, 0),
        }
    }

    /// The number of a vector register operand.
    fn vector(&self, operand: Option<Operand>) -> Run<usize> {
        match operand {
            Some(Operand::Vector { first, .. }) => Ok(usize::from(first)),
            _ => self.fail("an operand that should be a vector register is not"),
        }
    }

    /// How many registers a lane mask takes.
    fn mask_words(&self) -> usize {
        if self.wave.wide { 2 } else { 1 }
    }

    /// Runs one instruction.
    fn step(&mut self, instruction: &Instruction) -> Run<Flow> {
        let name = instruction.mnemonic.unwrap_or("?");
        match instruction.format {
            Format::Sopp => self.control(instruction, name),
            Format::Sop2 | Format::Sop1 | Format::Sopc | Format::Sopk => {
                self.scalar(instruction, name)
            }
            Format::Vop1
            | Format::Vop2
            | Format::Vop3a
            | Format::Vop3b
            | Format::Vopc
            | Format::Vop3p => self.vector_op(instruction, name).map(|()| Flow::Next),
            Format::Vintrp => self.interpolate(instruction, name).map(|()| Flow::Next),
            Format::Smem | Format::Mubuf | Format::Mtbuf | Format::Global | Format::Flat => {
                self.memory(instruction, name).map(|()| Flow::Next)
            }
            Format::Ds => self.shared(instruction, name).map(|()| Flow::Next),
            Format::Mimg => self.image(instruction, name).map(|()| Flow::Next),
            Format::Exp => self.export(instruction).map(|()| Flow::Next),
            _ => self.fail("a format the machine has no rule for"),
        }
    }

    fn control(&mut self, instruction: &Instruction, name: &str) -> Run<Flow> {
        let _ = instruction;
        let taken = match name {
            "s_nop" | "s_waitcnt" | "s_inst_prefetch" | "s_clause" => return Ok(Flow::Next),
            // One wave is the whole thread group here, and it is always
            // all at the same instruction.
            "s_barrier" => return Ok(Flow::Next),
            "s_branch" => true,
            "s_cbranch_scc0" => !self.wave.scc,
            "s_cbranch_scc1" => self.wave.scc,
            "s_cbranch_vccz" => self.wave.mask(VCC) == 0,
            "s_cbranch_vccnz" => self.wave.mask(VCC) != 0,
            "s_cbranch_execz" => self.wave.exec() == 0,
            "s_cbranch_execnz" => self.wave.exec() != 0,
            "s_endpgm" => {
                if self.stage == HardwareStage::Pixel && !self.wave.exported {
                    for lane in 0..self.lanes {
                        self.host.discard(lane);
                    }
                }
                return Ok(Flow::End);
            }
            _ => return self.fail("a program-control instruction the machine has no rule for"),
        };
        Ok(if taken { Flow::Branch } else { Flow::Next })
    }

    fn scalar(&mut self, instruction: &Instruction, name: &str) -> Run<Flow> {
        let target = instruction.arg(Role::Result);
        let sources: Vec<Operand> = instruction.sources().collect();
        let immediate = match instruction.detail {
            Detail::Immediate(immediate) => immediate,
            _ => 0,
        };
        let simm = immediate as i16 as i32 as u32;
        let source = |index: usize| sources.get(index).copied().unwrap_or(Operand::Null);
        let lane_mask = if self.wave.wide {
            u64::MAX
        } else {
            u64::from(u32::MAX)
        };

        // Bitwise families, on 32 or 64 bits as plain numbers.
        if let Some((base, ty)) = name.rsplit_once('_')
            && let Some(base) = base.strip_prefix("s_")
            && matches!(ty, "b32" | "b64")
        {
            let width = if ty == "b64" { 2 } else { 1 };
            let all = if width == 2 { u64::MAX } else { 0xFFFF_FFFF };
            let apply = |op: &str, a: u64, b: u64| -> Option<u64> {
                Some(
                    match op {
                        "and" => a & b,
                        "or" => a | b,
                        "xor" => a ^ b,
                        "andn2" => a & !b,
                        "orn2" => a | !b,
                        "nand" => !(a & b),
                        "nor" => !(a | b),
                        "xnor" => !(a ^ b),
                        "andn1" => !a & b,
                        "orn1" => !a | b,
                        _ => return None,
                    } & all,
                )
            };
            if apply(base, 0, 0).is_some() {
                let a = self.read_wide(source(0), width)?;
                let b = self.read_wide(source(1), width)?;
                let result = apply(base, a, b).unwrap_or(0);
                self.write_wide(target, width, result)?;
                self.wave.scc = result != 0;
                return Ok(Flow::Next);
            }
            let saving = base.strip_suffix("_saveexec");
            let writing = base.strip_suffix("_wrexec");
            if let Some(op) = saving.or(writing)
                && apply(op, 0, 0).is_some()
            {
                let operand = self.read_wide(source(0), width)?;
                let old = self.wave.exec();
                let new = apply(op, operand, old).unwrap_or(0) & lane_mask;
                self.write_wide(target, width, if saving.is_some() { old } else { new })?;
                self.wave.set_mask(EXEC, new);
                self.wave.scc = new != 0;
                return Ok(Flow::Next);
            }
        }

        let a = |machine: &Self| machine.read(source(0), 0);
        let b = |machine: &Self| machine.read(source(1), 0);
        match name {
            "s_mov_b32" => {
                let value = a(self)?;
                self.write_wide(target, 1, u64::from(value))?;
            }
            "s_mov_b64" => {
                let value = self.read_wide(source(0), 2)?;
                self.write_wide(target, 2, value)?;
            }
            "s_cmov_b32" | "s_cmov_b64" => {
                let width = if name == "s_cmov_b64" { 2 } else { 1 };
                if self.wave.scc {
                    let value = self.read_wide(source(0), width)?;
                    self.write_wide(target, width, value)?;
                }
            }
            "s_cselect_b32" | "s_cselect_b64" => {
                let width = if name == "s_cselect_b64" { 2 } else { 1 };
                let chosen = if self.wave.scc { source(0) } else { source(1) };
                let value = self.read_wide(chosen, width)?;
                self.write_wide(target, width, value)?;
            }
            "s_not_b32" | "s_not_b64" => {
                let width = if name == "s_not_b64" { 2 } else { 1 };
                let all = if width == 2 { u64::MAX } else { 0xFFFF_FFFF };
                let value = !self.read_wide(source(0), width)? & all;
                self.write_wide(target, width, value)?;
                self.wave.scc = value != 0;
            }
            "s_wqm_b32" | "s_wqm_b64" => {
                let width = if name == "s_wqm_b64" { 2 } else { 1 };
                let value = self.read_wide(source(0), width)?;
                let mut whole = 0u64;
                for quad in 0..16 {
                    if value >> (4 * quad) & 0xF != 0 {
                        whole |= 0xF << (4 * quad);
                    }
                }
                // Lanes that do not exist stay out.
                let whole = whole & self.wave.started;
                self.write_wide(target, width, whole)?;
                self.wave.scc = whole != 0;
            }
            "s_movk_i32" => self.write_wide(target, 1, u64::from(simm))?,
            "s_cmovk_i32" => {
                if self.wave.scc {
                    self.write_wide(target, 1, u64::from(simm))?;
                }
            }
            "s_add_u32" | "s_add_i32" | "s_sub_u32" | "s_sub_i32" | "s_addk_i32" => {
                let (x, y) = if name == "s_addk_i32" {
                    (self.read(target.unwrap_or(Operand::Null), 0)?, simm)
                } else {
                    (a(self)?, b(self)?)
                };
                let (result, condition) = match name {
                    "s_add_u32" => (x.wrapping_add(y), x.checked_add(y).is_none()),
                    "s_sub_u32" => (x.wrapping_sub(y), y > x),
                    "s_sub_i32" => {
                        let (result, overflow) = (x as i32).overflowing_sub(y as i32);
                        (result as u32, overflow)
                    }
                    _ => {
                        let (result, overflow) = (x as i32).overflowing_add(y as i32);
                        (result as u32, overflow)
                    }
                };
                self.write_wide(target, 1, u64::from(result))?;
                self.wave.scc = condition;
            }
            "s_addc_u32" => {
                let sum = u64::from(a(self)?) + u64::from(b(self)?) + u64::from(self.wave.scc);
                self.write_wide(target, 1, sum & 0xFFFF_FFFF)?;
                self.wave.scc = sum >> 32 != 0;
            }
            "s_subb_u32" => {
                let (x, y) = (u64::from(a(self)?), u64::from(b(self)?));
                let take = y + u64::from(self.wave.scc);
                self.write_wide(target, 1, x.wrapping_sub(take) & 0xFFFF_FFFF)?;
                self.wave.scc = take > x;
            }
            "s_min_i32" | "s_min_u32" | "s_max_i32" | "s_max_u32" => {
                let (x, y) = (a(self)?, b(self)?);
                let first = match name {
                    "s_min_i32" => (x as i32) < (y as i32),
                    "s_min_u32" => x < y,
                    "s_max_i32" => (x as i32) > (y as i32),
                    _ => x > y,
                };
                self.write_wide(target, 1, u64::from(if first { x } else { y }))?;
                self.wave.scc = first;
            }
            "s_mul_i32" => {
                let value = a(self)?.wrapping_mul(b(self)?);
                self.write_wide(target, 1, u64::from(value))?;
            }
            "s_mul_hi_u32" => {
                let value = (u64::from(a(self)?) * u64::from(b(self)?)) >> 32;
                self.write_wide(target, 1, value)?;
            }
            "s_mulk_i32" => {
                let value = self
                    .read(target.unwrap_or(Operand::Null), 0)?
                    .wrapping_mul(simm);
                self.write_wide(target, 1, u64::from(value))?;
            }
            "s_lshl_b32" | "s_lshr_b32" | "s_ashr_i32" => {
                let (x, count) = (a(self)?, b(self)? & 31);
                let value = match name {
                    "s_lshl_b32" => x << count,
                    "s_lshr_b32" => x >> count,
                    _ => ((x as i32) >> count) as u32,
                };
                self.write_wide(target, 1, u64::from(value))?;
                self.wave.scc = value != 0;
            }
            "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" => {
                let x = self.read_wide(source(0), 2)?;
                let count = b(self)? & 63;
                let value = match name {
                    "s_lshl_b64" => x << count,
                    "s_lshr_b64" => x >> count,
                    _ => ((x as i64) >> count) as u64,
                };
                self.write_wide(target, 2, value)?;
                self.wave.scc = value != 0;
            }
            "s_bfe_u32" | "s_bfe_i32" => {
                let (x, field) = (a(self)?, b(self)?);
                let (offset, width) = (field & 31, field >> 16 & 0x7F);
                let shifted = u64::from(x >> offset);
                let value = if width == 0 {
                    0
                } else if width >= 32 {
                    if name == "s_bfe_i32" {
                        ((x as i32) >> offset) as u32
                    } else {
                        x >> offset
                    }
                } else {
                    let kept = (shifted & ((1u64 << width) - 1)) as u32;
                    if name == "s_bfe_i32" && kept >> (width - 1) & 1 != 0 {
                        kept | (u32::MAX << width)
                    } else {
                        kept
                    }
                };
                self.write_wide(target, 1, u64::from(value))?;
                self.wave.scc = value != 0;
            }
            "s_brev_b32" => {
                let value = a(self)?.reverse_bits();
                self.write_wide(target, 1, u64::from(value))?;
            }
            "s_bcnt1_i32_b32" | "s_bcnt1_i32_b64" => {
                let width = if name.ends_with("b64") { 2 } else { 1 };
                let value = self.read_wide(source(0), width)?.count_ones();
                self.write_wide(target, 1, u64::from(value))?;
                self.wave.scc = value != 0;
            }
            "s_ff1_i32_b32" | "s_ff1_i32_b64" => {
                let width = if name.ends_with("b64") { 2 } else { 1 };
                let bits = self.read_wide(source(0), width)?;
                let value = if bits == 0 {
                    u32::MAX
                } else {
                    bits.trailing_zeros()
                };
                self.write_wide(target, 1, u64::from(value))?;
            }
            "s_cmp_eq_u64" | "s_cmp_lg_u64" => {
                let equal = self.read_wide(source(0), 2)? == self.read_wide(source(1), 2)?;
                self.wave.scc = equal == (name == "s_cmp_eq_u64");
            }
            "s_getpc_b64" => {
                self.write_wide(target, 2, CODE_BASE + u64::from(instruction.end()))?;
            }
            "s_setpc_b64" => return Ok(Flow::FarJump),
            "s_swappc_b64" => {
                let StageData::Vertex(vertex) = self.data else {
                    return self.fail("a call in a program that is not a vertex program");
                };
                for input in &vertex.inputs {
                    for channel in 0..4u8 {
                        if input.mask >> channel & 1 == 0 {
                            continue;
                        }
                        for lane in 0..self.lanes {
                            self.wave.v[usize::from(input.register) + usize::from(channel)][lane] =
                                self.host.input(
                                    lane,
                                    Input::VertexAttribute {
                                        row: input.row,
                                        channel,
                                    },
                                );
                        }
                    }
                }
            }
            "s_setreg_imm32_b32" => {
                let (register, offset, size) = (
                    immediate & 0x3F,
                    u32::from(immediate >> 6 & 0x1F),
                    u32::from(immediate >> 11 & 0x1F) + 1,
                );
                let value = a(self)?;
                // Bits 2 and 3 of the float mode register round 16-bit
                // floats.
                if register == 1 && offset <= 2 && offset + size >= 4 {
                    self.wave.half_toward_zero = value >> (2 - offset) & 3 == 3;
                }
            }
            _ => {
                let compare = name.strip_prefix("s_cmp_").or(name.strip_prefix("s_cmpk_"));
                let Some((relation, ty)) = compare.and_then(|rest| rest.split_once('_')) else {
                    return self.fail("a scalar operation the machine has no rule for");
                };
                let x = a(self)?;
                let y = if name.starts_with("s_cmpk_") {
                    if ty == "i32" {
                        simm
                    } else {
                        u32::from(immediate)
                    }
                } else {
                    b(self)?
                };
                let ordering = if ty == "i32" {
                    (x as i32).cmp(&(y as i32))
                } else {
                    x.cmp(&y)
                };
                use std::cmp::Ordering::{Equal, Greater, Less};
                self.wave.scc = match relation {
                    "eq" => ordering == Equal,
                    "lg" => ordering != Equal,
                    "gt" => ordering == Greater,
                    "ge" => ordering != Less,
                    "lt" => ordering == Less,
                    "le" => ordering != Greater,
                    _ => return self.fail("a scalar comparison the machine has no rule for"),
                };
            }
        }
        Ok(Flow::Next)
    }

    /// One source of a vector operation in one lane, through the
    /// sub-register select and the float modifiers.
    fn vector_source(
        &self,
        operand: Operand,
        index: usize,
        lane: usize,
        modifiers: &VectorModifiers,
        permuted: Option<Option<u32>>,
    ) -> Run<Option<u32>> {
        let mut value = match permuted {
            Some(Some(value)) => value,
            Some(None) => return Ok(None),
            None => self.lane_word(operand, lane)?,
        };
        if let (Some(sub), true) = (modifiers.sub_register, index < 2) {
            let select = sub.source_select[index];
            let (shift, bits) = match select {
                0..=3 => (8 * u32::from(select), 8),
                4 | 5 => (16 * u32::from(select - 4), 16),
                _ => (0, 32),
            };
            if bits < 32 {
                let part = value >> shift & ((1 << bits) - 1);
                value = if sub.source_sign_extend[index] && part >> (bits - 1) & 1 != 0 {
                    part | (u32::MAX << bits)
                } else {
                    part
                };
            }
        }
        if modifiers.absolute >> index & 1 != 0 {
            value &= 0x7FFF_FFFF;
        }
        if modifiers.negate >> index & 1 != 0 {
            value ^= 0x8000_0000;
        }
        Ok(Some(value))
    }

    /// Source 0 as each lane sees it through a lane permutation: `None` for
    /// a lane that is not written. `before` holds the source register as
    /// it was when the instruction began, since the instruction may write
    /// the register it reads.
    fn permuted(
        &self,
        before: &[u32; 64],
        permutation: LanePermutation,
        lane: usize,
    ) -> Run<Option<u32>> {
        // The lane read, if the permutation names one inside its row; and
        // what happens to a lane whose read finds nothing.
        let (source, fetch_inactive, bound_control) = match permutation {
            // Any lane of the same group of eight.
            LanePermutation::Eight {
                lanes,
                fetch_inactive,
            } => (
                Some((lane & !7) + usize::from(lanes[lane & 7] & 7)),
                fetch_inactive,
                false,
            ),
            LanePermutation::Rows {
                control,
                fetch_inactive,
                bound_control,
                bank_mask,
                row_mask,
            } => {
                if bank_mask != 0xF || row_mask != 0xF {
                    return self.fail("a permutation the machine has no rule for");
                }
                let place = (lane % 16) as i32;
                let by = i32::from(control & 0xF);
                let from = match control {
                    // Within the group of four.
                    0x000..=0x0FF => (place & !3) + i32::from(control >> (2 * (lane & 3)) & 3),
                    // The row moved left: a lane gets the value from above.
                    0x101..=0x10F => place + by,
                    // The row moved right: a lane gets the value from below.
                    0x111..=0x11F => place - by,
                    // The same going round.
                    0x121..=0x12F => (place - by + 16) % 16,
                    // The row back to front, and each half of it.
                    0x140 => 15 - place,
                    0x141 => (place & 8) + (7 - (place & 7)),
                    _ => return self.fail("a permutation the machine has no rule for"),
                };
                let source = (0..16)
                    .contains(&from)
                    .then(|| lane - lane % 16 + from as usize);
                (source, fetch_inactive, bound_control)
            }
        };
        let found = source
            .filter(|&source| fetch_inactive || self.wave.exec() >> source & 1 != 0)
            .filter(|&source| source < 32 * self.mask_words());
        Ok(match found {
            Some(source) => Some(before[source]),
            None if bound_control => Some(0),
            None => None,
        })
    }

    // The comparisons are written out as the instruction set defines them:
    // with a NaN operand "not less" is true and "less or greater" is false,
    // which the shorter forms a linter suggests would get wrong.
    #[allow(clippy::double_comparisons, clippy::neg_cmp_op_on_partial_ord)]
    fn vector_op(&mut self, instruction: &Instruction, name: &str) -> Run<()> {
        if matches!(name, "v_nop" | "v_pipeflush" | "v_clrexcp") {
            return Ok(());
        }
        let modifiers = match instruction.detail {
            Detail::Vector(modifiers) => modifiers,
            _ => VectorModifiers::default(),
        };
        let sources: Vec<Operand> = instruction.sources().collect();
        let source = |index: usize| sources.get(index).copied().unwrap_or(Operand::Null);
        let target = instruction.arg(Role::Result);
        let words = self.mask_words();
        let live = self.live_lanes();

        match name {
            "v_readfirstlane_b32" => {
                let exec = self.wave.exec();
                let lane = if exec == 0 {
                    0
                } else {
                    exec.trailing_zeros() as usize
                };
                let value = self.lane_word(source(0), lane)?;
                return self.write_wide(target, 1, u64::from(value));
            }
            "v_readlane_b32" => {
                let lane = self.read(source(1), 0)? as usize % (32 * words);
                let value = self.lane_word(source(0), lane)?;
                return self.write_wide(target, 1, u64::from(value));
            }
            "v_writelane_b32" => {
                let register = self.vector(target)?;
                let value = self.read(source(0), 0)?;
                let lane = self.read(source(1), 0)? as usize % (32 * words);
                self.wave.v[register][lane] = value;
                return Ok(());
            }
            "v_cndmask_b32" => {
                let register = self.vector(target)?;
                let mask = self.read_wide(source(2), words)?;
                for lane in live {
                    let pick = usize::from(mask >> lane & 1 != 0);
                    let Some(value) =
                        self.vector_source(source(pick), pick, lane, &modifiers, None)?
                    else {
                        continue;
                    };
                    self.wave.v[register][lane] = value;
                }
                return Ok(());
            }
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" | "v_add_co_ci_u32"
            | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32" => {
                let register = self.vector(target)?;
                let carry_in = match sources.get(2) {
                    Some(&operand) => self.read_wide(operand, words)?,
                    None => 0,
                };
                let mut carry_out = 0u64;
                for lane in live {
                    let mut x = u64::from(self.lane_word(source(0), lane)?);
                    let mut y = u64::from(self.lane_word(source(1), lane)?);
                    if name.contains("subrev") {
                        std::mem::swap(&mut x, &mut y);
                    }
                    let carry = carry_in >> lane & 1;
                    let (value, out) = if name.contains("sub") {
                        (x.wrapping_sub(y).wrapping_sub(carry), y + carry > x)
                    } else {
                        (x + y + carry, (x + y + carry) >> 32 != 0)
                    };
                    self.wave.v[register][lane] = value as u32;
                    carry_out |= u64::from(out) << lane;
                }
                return self.write_wide(instruction.arg(Role::SecondResult), words, carry_out);
            }
            "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => {
                // A register pair shifted by the low six bits of the first
                // operand.
                let register = self.vector(target)?;
                for lane in live {
                    let by = self.lane_word(source(0), lane)? & 63;
                    let value = match source(1) {
                        Operand::Vector { first, .. } => {
                            u64::from(self.wave.v[usize::from(first)][lane])
                                | u64::from(self.wave.v[usize::from(first) + 1][lane]) << 32
                        }
                        other => self.read_wide(other, 2)?,
                    };
                    let shifted = match name {
                        "v_lshlrev_b64" => value << by,
                        "v_lshrrev_b64" => value >> by,
                        _ => ((value as i64) >> by) as u64,
                    };
                    self.wave.v[register][lane] = shifted as u32;
                    self.wave.v[register + 1][lane] = (shifted >> 32) as u32;
                }
                return Ok(());
            }
            "v_permlanex16_b32" => {
                // Each lane reads a lane of the other row of sixteen in its
                // half of the wave: which one is the lane's four bits of the
                // two selector words. Bit 0 of the select field fetches
                // lanes that are not live; without it such a read gives
                // zero if bit 1 is set and otherwise leaves the lane as it
                // was.
                let register = self.vector(target)?;
                let before = self.wave.v[self.vector(Some(source(0)))?];
                let picks =
                    u64::from(self.read(source(1), 0)?) | u64::from(self.read(source(2), 0)?) << 32;
                let exec = self.wave.exec();
                for lane in live {
                    let other = lane ^ 16;
                    let from = other - other % 16 + (picks >> (4 * (lane % 16)) & 0xF) as usize;
                    if modifiers.select & 1 != 0 || exec >> from & 1 != 0 {
                        self.wave.v[register][lane] = before[from];
                    } else if modifiers.select & 2 != 0 {
                        self.wave.v[register][lane] = 0;
                    }
                }
                return Ok(());
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
                let register = self.vector(target)?;
                let mask = u64::from(self.read(source(0), 0)?);
                for lane in live {
                    // The low instruction counts lanes 0 to 31 below this
                    // lane, the high one lanes 32 to 63.
                    let below = if name == "v_mbcnt_lo_u32_b32" {
                        mask & ((1u64 << lane.min(32)) - 1)
                    } else {
                        mask & ((1u64 << lane.saturating_sub(32)) - 1)
                    };
                    let add = self.lane_word(source(1), lane)?;
                    self.wave.v[register][lane] = below.count_ones().wrapping_add(add);
                }
                return Ok(());
            }
            _ => {}
        }

        if name.starts_with("v_cmp") {
            let into_live = name.starts_with("v_cmpx_");
            let rest = name
                .strip_prefix("v_cmpx_")
                .or(name.strip_prefix("v_cmp_"))
                .unwrap_or("");
            let Some((relation, ty)) = rest.rsplit_once('_') else {
                return self.fail("a comparison the machine has no rule for");
            };
            let mut result = 0u64;
            for lane in live {
                let Some(x) = self.vector_source(source(0), 0, lane, &modifiers, None)? else {
                    continue;
                };
                let Some(y) = self.vector_source(source(1), 1, lane, &modifiers, None)? else {
                    continue;
                };
                let holds = match ty {
                    "f32" => {
                        let (x, y) = (f32::from_bits(x), f32::from_bits(y));
                        match relation {
                            "f" => false,
                            "lt" => x < y,
                            "eq" => x == y,
                            "le" => x <= y,
                            "gt" => x > y,
                            "lg" => x < y || x > y,
                            "ge" => x >= y,
                            "o" => !x.is_nan() && !y.is_nan(),
                            "u" => x.is_nan() || y.is_nan(),
                            "nge" => !(x >= y),
                            "nlg" => !(x < y || x > y),
                            "ngt" => !(x > y),
                            "nle" => !(x <= y),
                            "neq" => x != y,
                            "nlt" => !(x < y),
                            "tru" => true,
                            "class" => eval::float_class(x.to_bits(), y.to_bits()),
                            _ => return self.fail("a comparison the machine has no rule for"),
                        }
                    }
                    "i32" | "u32" => {
                        let ordering = if ty == "i32" {
                            (x as i32).cmp(&(y as i32))
                        } else {
                            x.cmp(&y)
                        };
                        use std::cmp::Ordering::{Equal, Greater, Less};
                        match relation {
                            "f" => false,
                            "lt" => ordering == Less,
                            "eq" => ordering == Equal,
                            "le" => ordering != Greater,
                            "gt" => ordering == Greater,
                            "ne" => ordering != Equal,
                            "ge" => ordering != Less,
                            "t" => true,
                            _ => return self.fail("a comparison the machine has no rule for"),
                        }
                    }
                    _ => return self.fail("a comparison the machine has no rule for"),
                };
                result |= u64::from(holds) << lane;
            }
            if into_live {
                self.wave.set_mask(EXEC, result);
                return Ok(());
            }
            return self.write_wide(target, words, result);
        }

        let register = self.vector(target)?;
        let mix = matches!(
            name,
            "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16"
        );
        // Every lane reads before any lane writes.
        let before = match (modifiers.permutation, sources.first()) {
            (Some(_), Some(&Operand::Vector { first, .. })) => self.wave.v[usize::from(first)],
            (Some(_), _) => return self.fail("a permutation of something not a register"),
            (None, _) => [0; 64],
        };
        for lane in live {
            let mut values = [0u32; 3];
            let mut written = true;
            for (index, &operand) in sources.iter().enumerate().take(3) {
                let permuted = match (index, modifiers.permutation) {
                    (0, Some(permutation)) => Some(self.permuted(&before, permutation, lane)?),
                    _ => None,
                };
                if mix {
                    // The mix operations keep their operand kinds in the
                    // packed fields; see the lift.
                    let mut value = self.lane_word(operand, lane)?;
                    if modifiers.select_high >> index & 1 != 0 {
                        if modifiers.select >> index & 1 != 0 {
                            value >>= 16;
                        }
                        value = eval::float_from_half(value as u16).to_bits();
                    }
                    if modifiers.negate_high >> index & 1 != 0 {
                        value &= 0x7FFF_FFFF;
                    }
                    if modifiers.negate >> index & 1 != 0 {
                        value ^= 0x8000_0000;
                    }
                    values[index] = value;
                    continue;
                }
                match self.vector_source(operand, index, lane, &modifiers, permuted)? {
                    Some(value) => values[index] = value,
                    None => written = false,
                }
            }
            if !written {
                continue;
            }
            let old = self.wave.v[register][lane];
            if matches!(name, "v_mac_f32" | "v_fmac_f32") {
                values[2] = old;
            }
            let Some(mut value) = vector_result(name, values, self.wave.half_toward_zero) else {
                return self.fail("a vector operation the machine has no rule for");
            };
            if modifiers.output_scale != 0 {
                let scale = [1.0, 2.0, 4.0, 0.5][usize::from(modifiers.output_scale & 3)];
                value = (f32::from_bits(value) * scale).to_bits();
            }
            if modifiers.clamp {
                let float = f32::from_bits(value);
                value = if float.is_nan() {
                    0
                } else {
                    float.clamp(0.0, 1.0).to_bits()
                };
            }
            let half = match name {
                "v_cvt_f16_f32" | "v_fma_mixlo_f16" | "v_lshrrev_b16" | "v_lshlrev_b16" => {
                    Some(false)
                }
                "v_fma_mixhi_f16" => Some(true),
                _ => None,
            };
            if let Some(sub) = modifiers.sub_register {
                let (shift, bits) = match sub.result_select {
                    0..=3 => (8 * u32::from(sub.result_select), 8),
                    4 | 5 => (16 * u32::from(sub.result_select - 4), 16),
                    _ => (0, 32),
                };
                if bits < 32 {
                    let field = (1u32 << bits) - 1;
                    let placed = (value & field) << shift;
                    value = match sub.result_unused {
                        0 => placed,
                        1 => {
                            let top = 32 - bits - shift;
                            (((placed << top) as i32) >> top) as u32
                        }
                        _ => old & !(field << shift) | placed,
                    };
                }
            } else if let Some(high) = half {
                value = if high {
                    old & 0xFFFF | value << 16
                } else {
                    old & 0xFFFF_0000 | value
                };
            }
            self.wave.v[register][lane] = value;
        }
        Ok(())
    }

    fn interpolate(&mut self, instruction: &Instruction, name: &str) -> Run<()> {
        let Detail::Interpolation { attribute, channel } = instruction.detail else {
            return self.fail("an interpolation without its fields");
        };
        let register = self.vector(instruction.arg(Role::Result))?;
        let source = instruction.arg(Role::Source).unwrap_or(Operand::Null);
        for lane in self.live_lanes() {
            let mut term = |term| {
                f32::from_bits(self.host.input(
                    lane,
                    Input::Attribute {
                        attribute,
                        channel,
                        term,
                    },
                ))
            };
            let value = match name {
                "v_interp_p1_f32" => {
                    let (slope, base) = (term(VertexTerm::P10), term(VertexTerm::P0));
                    let weight = f32::from_bits(self.lane_word(source, lane)?);
                    slope.mul_add(weight, base)
                }
                "v_interp_p2_f32" => {
                    let slope = term(VertexTerm::P20);
                    let weight = f32::from_bits(self.lane_word(source, lane)?);
                    slope.mul_add(weight, f32::from_bits(self.wave.v[register][lane]))
                }
                "v_interp_mov_f32" => match source {
                    Operand::Immediate(0) => term(VertexTerm::P10),
                    Operand::Immediate(1) => term(VertexTerm::P20),
                    _ => term(VertexTerm::P0),
                },
                _ => return self.fail("an interpolation the machine has no rule for"),
            };
            self.wave.v[register][lane] = value.to_bits();
        }
        Ok(())
    }

    /// The four words of a descriptor in scalar registers.
    fn descriptor(&self, operand: Option<Operand>) -> Run<[u32; 4]> {
        match operand.and_then(code_of) {
            Some(code) => Ok([
                self.wave.s[code],
                self.wave.s[code + 1],
                self.wave.s[code + 2],
                self.wave.s[code + 3],
            ]),
            None => self.fail("a descriptor that is not in scalar registers"),
        }
    }

    fn memory(&mut self, instruction: &Instruction, name: &str) -> Run<()> {
        let units = |name: &str| -> usize {
            for (suffix, count) in [("x16", 16), ("x8", 8), ("x4", 4), ("x3", 3), ("x2", 2)] {
                if name.ends_with(suffix) {
                    return count;
                }
            }
            1
        };
        match instruction.detail {
            Detail::ScalarMemory { offset, .. } => {
                let Some(target) = instruction.arg(Role::Result).and_then(code_of) else {
                    return self.fail("a scalar load without a result");
                };
                let mut position = offset as u32;
                if let Some(register) = instruction.arg(Role::Offset) {
                    position = position.wrapping_add(self.read(register, 0)?);
                }
                let count = units(name);
                let mut values = Vec::new();
                if name.starts_with("s_buffer_load_dword") {
                    let view =
                        BufferView::from_words(self.descriptor(instruction.arg(Role::Base))?);
                    for unit in 0..count {
                        let at = (position & !3).wrapping_add(4 * unit as u32);
                        values.push(match view.locate(0, at) {
                            Some(address) => self.host.load(address, 32),
                            None => 0,
                        });
                    }
                } else if name.starts_with("s_load_dword") {
                    let base = instruction.arg(Role::Base).unwrap_or(Operand::Null);
                    let address = self.read_wide(base, 2)?.wrapping_add(u64::from(position)) & !3;
                    for unit in 0..count {
                        let at = address.wrapping_add(4 * unit as u64);
                        // A load from the program's own address range reads
                        // the constants after the code.
                        let in_code = at
                            .checked_sub(CODE_BASE)
                            .filter(|offset| *offset < 4 * self.code.len() as u64);
                        values.push(match in_code {
                            Some(offset) => self.code[(offset / 4) as usize],
                            None => self.host.load(at, 32),
                        });
                    }
                } else {
                    return self.fail("a scalar memory operation the machine has no rule for");
                }
                for (unit, value) in values.into_iter().enumerate() {
                    self.wave.s[target + unit] = value;
                }
                Ok(())
            }
            Detail::Buffer {
                offset,
                offset_enable,
                index_enable,
                format,
                ..
            } => {
                let view = self.descriptor(instruction.arg(Role::Resource))?;
                let format_code = match format {
                    Some(code) => u32::from(code),
                    None => view[3] >> 12 & 0x7F,
                };
                let view = BufferView::from_words(view);
                let address = match instruction.arg(Role::Address) {
                    Some(Operand::Vector { first, .. }) => Some(usize::from(first)),
                    _ => None,
                };
                let extra = match instruction.arg(Role::Offset) {
                    Some(register) => self.read(register, 0)?,
                    None => 0,
                };
                let typed = name.contains("_format_");
                let load = name.contains("_load_");
                let data =
                    self.vector(instruction.arg(if load { Role::Result } else { Role::Data }))?;
                if name.contains("_atomic_") {
                    let Detail::Buffer { glc, .. } = instruction.detail else {
                        return self.fail("a buffer operation without its fields");
                    };
                    for lane in self.live_lanes() {
                        let mut index = 0;
                        let mut position = u32::from(offset).wrapping_add(extra);
                        if let Some(mut register) = address {
                            if index_enable {
                                index = self.wave.v[register][lane];
                                register += 1;
                            }
                            if offset_enable {
                                position = position.wrapping_add(self.wave.v[register][lane]);
                            }
                        }
                        let old = match view.locate(index, position) {
                            Some(at) => self.atomic(name, at, data, lane)?,
                            None => 0,
                        };
                        if glc {
                            self.wave.v[data][lane] = old;
                        }
                    }
                    return Ok(());
                }
                let channels = match name.rsplit('_').next() {
                    Some("x") => 1,
                    Some("xy") => 2,
                    Some("xyz") => 3,
                    _ => 4,
                };
                for lane in self.live_lanes() {
                    let mut index = 0;
                    let mut position = u32::from(offset).wrapping_add(extra);
                    if let Some(mut register) = address {
                        if index_enable {
                            index = self.wave.v[register][lane];
                            register += 1;
                        }
                        if offset_enable {
                            position = position.wrapping_add(self.wave.v[register][lane]);
                        }
                    }
                    if typed {
                        let Some(format) = BufferFormat::from_code(format_code) else {
                            return self.fail("a data format the machine does not know");
                        };
                        let place = view.locate(index, position);
                        if load {
                            let values = match place {
                                Some(at) => {
                                    let mut raw = 0u128;
                                    for byte in 0..format.bytes() {
                                        let value =
                                            self.host.load(at.wrapping_add(u64::from(byte)), 8);
                                        raw |= u128::from(value & 0xFF) << (8 * byte);
                                    }
                                    format.decode(raw)
                                }
                                None => [0; 4],
                            };
                            for (channel, &value) in values.iter().enumerate().take(channels) {
                                self.wave.v[data + channel][lane] = value;
                            }
                        } else if let Some(at) = place {
                            let mut values = [0u32; 4];
                            for (channel, value) in values.iter_mut().enumerate().take(channels) {
                                *value = self.wave.v[data + channel][lane];
                            }
                            let Some(raw) = format.encode(values) else {
                                return self.fail("a data format the machine cannot write");
                            };
                            for byte in 0..format.bytes() {
                                self.host.store(
                                    at.wrapping_add(u64::from(byte)),
                                    8,
                                    (raw >> (8 * byte)) as u32 & 0xFF,
                                );
                            }
                        }
                        continue;
                    }
                    if !name.contains("dword") {
                        return self.fail("a buffer operation the machine has no rule for");
                    }
                    for unit in 0..units(name) {
                        let at = view.locate(index, position.wrapping_add(4 * unit as u32));
                        if load {
                            self.wave.v[data + unit][lane] = match at {
                                Some(at) => self.host.load(at, 32),
                                None => 0,
                            };
                        } else if let Some(at) = at {
                            self.host.store(at, 32, self.wave.v[data + unit][lane]);
                        }
                    }
                }
                Ok(())
            }
            Detail::Flat { offset, glc, .. } => {
                let load = name.contains("_load_");
                let atomic = name.contains("_atomic_");
                if !name.contains("dword") && !atomic {
                    return self.fail("a memory operation the machine has no rule for");
                }
                let address_register = self.vector(instruction.arg(Role::Address))?;
                let base = match instruction.arg(Role::Base) {
                    Some(base) => Some(self.read_wide(base, 2)?),
                    None => None,
                };
                let data =
                    self.vector(instruction.arg(if load { Role::Result } else { Role::Data }))?;
                for lane in self.live_lanes() {
                    let first = u64::from(self.wave.v[address_register][lane]);
                    let address = match base {
                        Some(base) => base.wrapping_add(first),
                        None => first | u64::from(self.wave.v[address_register + 1][lane]) << 32,
                    }
                    .wrapping_add(i64::from(offset) as u64);
                    if atomic {
                        let old = self.atomic(name, address, data, lane)?;
                        if let (true, Some(Operand::Vector { first, .. })) =
                            (glc, instruction.arg(Role::Result))
                        {
                            self.wave.v[usize::from(first)][lane] = old;
                        }
                        continue;
                    }
                    for unit in 0..units(name) {
                        let at = address.wrapping_add(4 * unit as u64);
                        if load {
                            self.wave.v[data + unit][lane] = self.host.load(at, 32);
                        } else {
                            self.host.store(at, 32, self.wave.v[data + unit][lane]);
                        }
                    }
                }
                Ok(())
            }
            _ => self.fail("a memory operation without its fields"),
        }
    }

    /// What an atomic operation makes of the word that was there and its
    /// operand: the part of the mnemonic after `atomic_` (or, for the
    /// group-shared memory, after `ds_`) says which.
    fn atomic_result(kind: &str, old: u32, value: u32, compare: u32) -> Option<u32> {
        Some(match kind {
            "swap" | "wrxchg" => value,
            "cmpswap" | "cmpst" => {
                if old == compare {
                    value
                } else {
                    old
                }
            }
            "add" => old.wrapping_add(value),
            "sub" => old.wrapping_sub(value),
            "smin" | "min_i" => (old as i32).min(value as i32) as u32,
            "umin" | "min_u" => old.min(value),
            "smax" | "max_i" => (old as i32).max(value as i32) as u32,
            "umax" | "max_u" => old.max(value),
            "and" => old & value,
            "or" => old | value,
            "xor" => old ^ value,
            "inc" => old.wrapping_add(1),
            "dec" => old.wrapping_sub(1),
            _ => return None,
        })
    }

    /// One lane's atomic operation on the word of memory at an address.
    /// The operand is in `data`; a compare-and-swap has the new word there
    /// and the comparand in the register after it. Returns the old word.
    fn atomic(&mut self, name: &str, address: u64, data: usize, lane: usize) -> Run<u32> {
        let kind = name.split_once("_atomic_").map_or("", |(_, kind)| kind);
        let at = address & !3;
        let old = self.host.load(at, 32);
        let value = self.wave.v[data][lane];
        let compare = self.wave.v[(data + 1) % 256][lane];
        match Self::atomic_result(kind, old, value, compare) {
            Some(new) => {
                self.host.store(at, 32, new);
                Ok(old)
            }
            None => self.fail("an atomic operation the machine has no rule for"),
        }
    }

    /// The group-shared memory: reads, writes and atomic operations at the
    /// lane's address plus a constant, one lane after another; and the lane
    /// swizzle that is encoded as one of its operations.
    fn shared(&mut self, instruction: &Instruction, name: &str) -> Run<()> {
        let Detail::Lds {
            offset0, offset1, ..
        } = instruction.detail
        else {
            return self.fail("a shared-memory operation without its fields");
        };
        if name == "ds_swizzle_b32" {
            return self.swizzle(instruction, offset0, offset1);
        }
        let Some((kind, ty)) = name
            .strip_prefix("ds_")
            .and_then(|rest| rest.rsplit_once('_'))
        else {
            return self.fail("a shared-memory operation the machine has no rule for");
        };
        // The size of a unit in bytes, and whether a read extends its sign.
        let (bytes, signed) = match ty {
            "b8" | "u8" => (1usize, false),
            "i8" => (1, true),
            "b16" | "u16" => (2, false),
            "i16" => (2, true),
            "b32" | "u32" | "i32" => (4, false),
            _ => return self.fail("a shared-memory operation the machine has no rule for"),
        };
        let address = self.vector(instruction.arg(Role::Address))?;
        // The forms on two addresses count their offsets in units, the
        // `st64` ones in 64 units.
        let (kind, offsets): (&str, Vec<usize>) = match kind {
            "read2" | "write2" => (
                kind.trim_end_matches('2'),
                vec![usize::from(offset0) * bytes, usize::from(offset1) * bytes],
            ),
            "read2st64" | "write2st64" => (
                &kind[..kind.len() - 5],
                vec![
                    usize::from(offset0) * bytes * 64,
                    usize::from(offset1) * bytes * 64,
                ],
            ),
            _ => (kind, vec![usize::from(offset0) | usize::from(offset1) << 8]),
        };
        let data: Vec<usize> = instruction
            .args
            .iter()
            .filter(|arg| arg.role == Role::Data)
            .filter_map(|arg| match arg.operand {
                Operand::Vector { first, .. } => Some(usize::from(first)),
                _ => None,
            })
            .collect();
        let result = match instruction.arg(Role::Result) {
            Some(Operand::Vector { first, .. }) => Some(usize::from(first)),
            _ => None,
        };
        let size = self.wave.shared.len();
        // Where a unit starts, if all of it is inside the memory.
        let inside = |start: usize, bytes: usize| start <= size && bytes <= size - start;
        for lane in self.live_lanes() {
            let base = self.wave.v[address][lane] as usize;
            match kind {
                "read" => {
                    let Some(result) = result else {
                        return self.fail("a shared-memory read without a result");
                    };
                    // Every address is read before a register is written.
                    let mut values = Vec::new();
                    for &offset in &offsets {
                        let start = (base as u32).wrapping_add(offset as u32) as usize;
                        let mut value = 0u32;
                        if inside(start, bytes) {
                            for byte in (0..bytes).rev() {
                                value = value << 8 | u32::from(self.wave.shared[start + byte]);
                            }
                        }
                        if signed && bytes < 4 && value >> (8 * bytes - 1) & 1 != 0 {
                            value |= u32::MAX << (8 * bytes);
                        }
                        values.push(value);
                    }
                    for (index, value) in values.into_iter().enumerate() {
                        self.wave.v[result + index][lane] = value;
                    }
                }
                "write" => {
                    for (&offset, &register) in offsets.iter().zip(&data) {
                        let start = (base as u32).wrapping_add(offset as u32) as usize;
                        if inside(start, bytes) {
                            let word = self.wave.v[register][lane].to_le_bytes();
                            self.wave.shared[start..start + bytes].copy_from_slice(&word[..bytes]);
                        }
                    }
                }
                _ => {
                    // An atomic operation on a word; the forms named `rtn`
                    // give the word as it was.
                    let returns = kind.ends_with("_rtn");
                    let kind = kind.trim_end_matches("_rtn");
                    let key = match (kind, ty) {
                        ("min" | "max", "i32") => format!("{kind}_i"),
                        ("min" | "max", "u32") => format!("{kind}_u"),
                        _ => kind.to_string(),
                    };
                    // A compare-and-store has the comparand first.
                    let (value, compare) = match (kind, data.as_slice()) {
                        ("cmpst", &[compare, new]) => {
                            (self.wave.v[new][lane], self.wave.v[compare][lane])
                        }
                        (_, &[value, ..]) => (self.wave.v[value][lane], 0),
                        _ => return self.fail("a shared-memory operation without its operand"),
                    };
                    let start = ((base as u32).wrapping_add(offsets[0] as u32) & !3) as usize;
                    let mut old = 0;
                    if bytes == 4 && inside(start, 4) {
                        let mut word = [0u8; 4];
                        word.copy_from_slice(&self.wave.shared[start..start + 4]);
                        old = u32::from_le_bytes(word);
                        let Some(new) = Self::atomic_result(&key, old, value, compare) else {
                            return self
                                .fail("a shared-memory operation the machine has no rule for");
                        };
                        self.wave.shared[start..start + 4].copy_from_slice(&new.to_le_bytes());
                    } else if bytes != 4 {
                        return self.fail("a shared-memory operation the machine has no rule for");
                    }
                    if let (true, Some(result)) = (returns, result) {
                        self.wave.v[result][lane] = old;
                    }
                }
            }
        }
        Ok(())
    }

    fn swizzle(&mut self, instruction: &Instruction, offset0: u8, offset1: u8) -> Run<()> {
        if offset1 & 0x80 == 0 {
            return self.fail("a shared-memory operation the machine has no rule for");
        }
        let target = self.vector(instruction.arg(Role::Result))?;
        let source = self.vector(instruction.arg(Role::Address))?;
        let exec = self.wave.exec();
        let before = self.wave.v[source];
        for lane in self.live_lanes() {
            let from = (lane & !3) + usize::from(offset0 >> (2 * (lane & 3)) & 3);
            self.wave.v[target][lane] = if exec >> from & 1 != 0 {
                before[from]
            } else {
                0
            };
        }
        Ok(())
    }

    fn image(&mut self, instruction: &Instruction, name: &str) -> Run<()> {
        let Detail::Image {
            component_mask,
            dimension,
            unnormalized,
            a16,
            ..
        } = instruction.detail
        else {
            return self.fail("an image operation without its fields");
        };
        let dimension = Dimension::from_code(dimension);
        let sampling = name
            .strip_prefix("image_sample")
            .or(name.strip_prefix("image_gather4"));
        let has =
            |token: &str| sampling.is_some_and(|rest| rest.split('_').any(|part| part == token));
        let resource = self.descriptor(instruction.arg(Role::Resource).map(
            |operand| match operand {
                Operand::Scalar { first, .. } => Operand::Scalar {
                    first: first + 4,
                    count: 4,
                },
                other => other,
            },
        ))?;
        let sampler = match instruction.arg(Role::Sampler) {
            Some(operand) => Some(self.descriptor(Some(operand))?),
            None => None,
        };
        let mut registers = Vec::new();
        for arg in &instruction.args {
            if let (Role::Address, Operand::Vector { first, count }) = (arg.role, arg.operand) {
                registers.extend((0..count).map(|index| usize::from(first) + usize::from(index)));
            }
        }
        let data = self.vector(
            instruction
                .arg(Role::Result)
                .or(instruction.arg(Role::Data)),
        )?;
        let stores = name.starts_with("image_store");
        for lane in self.live_lanes() {
            let mut address = registers
                .iter()
                .map(|&register| self.wave.v[register][lane]);
            // With 16-bit addresses everything from the gradients on is
            // packed two to a register: halves that are floats for
            // sampling and integers otherwise.
            let floats = sampling.is_some() || name == "image_get_lod";
            let mut whole = |pending: &mut Option<u32>| {
                *pending = None;
                address.next().unwrap_or(0)
            };
            // The offset, the bias and the reference have a register each;
            // the bias is a 16-bit float in its low half when addresses are
            // 16-bit.
            let offset = has("o").then(|| Value::Word(whole(&mut None)));
            let bias = has("b").then(|| {
                let register = whole(&mut None);
                Value::Word(if a16 {
                    eval::float_from_half(register as u16).to_bits()
                } else {
                    register
                })
            });
            let reference = has("c").then(|| Value::Word(whole(&mut None)));
            let mut pending: Option<u32>;
            let mut next = |pending: &mut Option<u32>| -> Value {
                if !a16 {
                    return Value::Word(whole(pending));
                }
                let half = match pending.take() {
                    Some(high) => high,
                    None => {
                        let register = whole(pending);
                        *pending = Some(register >> 16);
                        register & 0xFFFF
                    }
                };
                Value::Word(if floats {
                    eval::float_from_half(half as u16).to_bits()
                } else {
                    half
                })
            };
            let mut level = if has("lz") {
                Level::Zero
            } else {
                Level::Implicit
            };
            if has("d") {
                let count = dimension.gradient_components();
                pending = None;
                let dx = (0..count).map(|_| next(&mut pending)).collect();
                pending = None;
                let dy = (0..count).map(|_| next(&mut pending)).collect();
                level = Level::Gradients(dx, dy);
            }
            pending = None;
            let coordinates = if name == "image_get_resinfo" {
                vec![next(&mut pending)]
            } else {
                (0..dimension.coordinates())
                    .map(|_| next(&mut pending))
                    .collect()
            };
            if has("l") {
                level = Level::Explicit(next(&mut pending));
            }
            let level_clamp = has("cl").then(|| next(&mut pending));
            let mip = name.ends_with("_mip").then(|| next(&mut pending));
            let kind = if name.starts_with("image_gather4") {
                ImageKind::Gather {
                    channel: component_mask.trailing_zeros() as u8,
                }
            } else if sampling.is_some() {
                ImageKind::Sample
            } else if stores {
                let mut values = [Value::Word(0); 4];
                let mut register = data;
                for (channel, value) in values.iter_mut().enumerate() {
                    if component_mask >> channel & 1 != 0 {
                        *value = Value::Word(self.wave.v[register][lane]);
                        register += 1;
                    }
                }
                ImageKind::Store {
                    values,
                    guard: Value::Flag(true),
                }
            } else if name == "image_get_resinfo" {
                ImageKind::Info
            } else if name == "image_get_lod" {
                ImageKind::QueryLevel
            } else if name.starts_with("image_load") {
                ImageKind::Load
            } else {
                return self.fail("an image operation the machine has no rule for");
            };
            let gathers = matches!(kind, ImageKind::Gather { .. });
            let access = ImageOp {
                kind,
                dimension,
                texture: Value::Word(resource[1]),
                format: Value::Word(resource[2]),
                sampler: sampler.map(|words| Sampler::Heap(Value::Word(words[3] & 0xFFF))),
                compare: has("c"),
                offset,
                bias,
                reference,
                coordinates,
                level,
                level_clamp,
                mip,
                unnormalized,
            };
            let results = self.host.image(lane, &access);
            if stores {
                continue;
            }
            let mask = if gathers {
                0xF
            } else if component_mask == 0 {
                1
            } else {
                component_mask
            };
            let mut register = data;
            for (channel, &value) in results.iter().enumerate() {
                if mask >> channel & 1 != 0 {
                    self.wave.v[register][lane] = value;
                    register += 1;
                }
            }
        }
        Ok(())
    }

    fn export(&mut self, instruction: &Instruction) -> Run<()> {
        let Detail::Export {
            target,
            enabled,
            done,
            ..
        } = instruction.detail
        else {
            return self.fail("an export without its fields");
        };
        let pixel = self.stage == HardwareStage::Pixel;
        let target = match target {
            0..=7 if pixel => Some(ExportTarget::Color(target)),
            8 if pixel => Some(ExportTarget::Depth),
            9 => None,
            12..=16 => Some(ExportTarget::Position(target - 12)),
            32..=63 => Some(ExportTarget::Parameter(target - 32)),
            _ => return self.fail("an export target the machine has no rule for"),
        };
        let sources: Vec<Operand> = instruction.sources().collect();
        let exec = self.wave.exec();
        for lane in 0..self.lanes {
            let live = exec >> lane & 1 != 0;
            if live && let Some(target) = target {
                let mut values = [None; 4];
                let mut next = sources.iter();
                for (channel, value) in values.iter_mut().enumerate() {
                    if enabled >> channel & 1 != 0 {
                        let operand = next.next().copied().unwrap_or(Operand::Null);
                        *value = Some(self.lane_word(operand, lane)?);
                    }
                }
                self.host.export(lane, target, values);
            }
            if pixel && done && !live {
                self.host.discard(lane);
            }
        }
        if pixel && done {
            self.wave.exported = true;
        }
        Ok(())
    }
}

/// What a "words in, a word out" vector operation computes, written out
/// independently of the lift.
fn vector_result(name: &str, s: [u32; 3], half_toward_zero: bool) -> Option<u32> {
    let [a, b, c] = s;
    let (x, y, z) = (f32::from_bits(a), f32::from_bits(b), f32::from_bits(c));
    let float = |value: f32| Some(value.to_bits());
    let to_half = |value: f32| u32::from(eval::half_from_float(value, half_toward_zero));
    match name {
        "v_mov_b32" => Some(a),
        "v_add_f32" => float(x + y),
        "v_sub_f32" => float(x - y),
        "v_subrev_f32" => float(y - x),
        "v_mul_f32" => float(x * y),
        "v_mul_legacy_f32" => float(if x == 0.0 || y == 0.0 { 0.0 } else { x * y }),
        "v_min_f32" => Some(eval::binary(BinaryOp::FloatMin, a, b)),
        "v_max_f32" => Some(eval::binary(BinaryOp::FloatMax, a, b)),
        "v_mac_f32" | "v_mad_f32" | "v_madak_f32" | "v_madmk_f32" => float(x * y + z),
        "v_fmac_f32" | "v_fma_f32" | "v_fmaak_f32" | "v_fmamk_f32" | "v_fma_mix_f32" => {
            float(x.mul_add(y, z))
        }
        "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => Some(to_half(x.mul_add(y, z))),
        "v_mad_legacy_f32" => float(if x == 0.0 || y == 0.0 { 0.0 } else { x * y } + z),
        "v_med3_f32" => Some(eval::ternary(TernaryOp::FloatMedian, a, b, c)),
        "v_rcp_f32" | "v_rcp_iflag_f32" => float(1.0 / x),
        "v_rsq_f32" => Some(eval::unary(UnaryOp::ReciprocalSqrt, a)),
        "v_sqrt_f32" => float(x.sqrt()),
        "v_floor_f32" => float(x.floor()),
        "v_ceil_f32" => float(x.ceil()),
        "v_trunc_f32" => float(x.trunc()),
        "v_rndne_f32" => float(x.round_ties_even()),
        "v_fract_f32" => Some(eval::unary(UnaryOp::Fract, a)),
        "v_sin_f32" => Some(eval::unary(UnaryOp::SinTurns, a)),
        "v_cos_f32" => Some(eval::unary(UnaryOp::CosTurns, a)),
        "v_exp_f32" => Some(eval::unary(UnaryOp::Exp2, a)),
        "v_log_f32" => Some(eval::unary(UnaryOp::Log2, a)),
        "v_cvt_f32_i32" => float(a as i32 as f32),
        "v_cvt_f32_u32" => float(a as f32),
        "v_cvt_i32_f32" => Some(x as i32 as u32),
        "v_cvt_u32_f32" => Some(x as u32),
        "v_cvt_f16_f32" => Some(to_half(x)),
        "v_cvt_f32_f16" => float(eval::float_from_half(a as u16)),
        "v_cvt_f32_ubyte0" => float((a & 0xFF) as f32),
        "v_cvt_f32_ubyte1" => float((a >> 8 & 0xFF) as f32),
        "v_cvt_f32_ubyte2" => float((a >> 16 & 0xFF) as f32),
        "v_cvt_f32_ubyte3" => float((a >> 24) as f32),
        "v_cvt_pkrtz_f16_f32" => Some(
            u32::from(eval::half_from_float(x, true))
                | u32::from(eval::half_from_float(y, true)) << 16,
        ),
        "v_pack_b32_f16" => Some(a & 0xFFFF | b << 16),
        "v_cvt_pk_u8_f32" => {
            // To the nearest whole number, ties to even, within 0..255; a
            // NaN gives 0. Into byte `b` of `c`.
            let byte = if x.is_nan() {
                0
            } else {
                x.round_ties_even().clamp(0.0, 255.0) as u32
            };
            let shift = 8 * (b & 3);
            Some(c & !(0xFF << shift) | byte << shift)
        }
        "v_add_nc_u32" => Some(a.wrapping_add(b)),
        "v_sub_nc_u32" => Some(a.wrapping_sub(b)),
        "v_subrev_nc_u32" => Some(b.wrapping_sub(a)),
        "v_mul_lo_u32" | "v_mul_lo_i32" => Some(a.wrapping_mul(b)),
        "v_mul_hi_u32" => Some(((u64::from(a) * u64::from(b)) >> 32) as u32),
        "v_mul_u32_u24" => Some((a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF)),
        "v_mad_u32_u24" => Some((a & 0xFF_FFFF).wrapping_mul(b & 0xFF_FFFF).wrapping_add(c)),
        "v_and_b32" => Some(a & b),
        "v_or_b32" => Some(a | b),
        "v_xor_b32" => Some(a ^ b),
        "v_not_b32" => Some(!a),
        "v_lshrrev_b16" => Some((b & 0xFFFF) >> (a & 15)),
        "v_lshlrev_b16" => Some((b << (a & 15)) & 0xFFFF),
        "v_lshlrev_b32" => Some(b << (a & 31)),
        "v_lshrrev_b32" => Some(b >> (a & 31)),
        "v_ashrrev_i32" => Some(((b as i32) >> (a & 31)) as u32),
        "v_min_u32" => Some(a.min(b)),
        "v_max_u32" => Some(a.max(b)),
        "v_min_i32" => Some((a as i32).min(b as i32) as u32),
        "v_max_i32" => Some((a as i32).max(b as i32) as u32),
        "v_add3_u32" => Some(a.wrapping_add(b).wrapping_add(c)),
        "v_or3_b32" => Some(a | b | c),
        "v_and_or_b32" => Some(a & b | c),
        "v_xad_u32" => Some((a ^ b).wrapping_add(c)),
        "v_lshl_or_b32" => Some(a << (b & 31) | c),
        "v_lshl_add_u32" => Some((a << (b & 7)).wrapping_add(c)),
        "v_add_lshl_u32" => Some(a.wrapping_add(b) << (c & 7)),
        "v_bfe_u32" => {
            let width = c & 31;
            Some(a >> (b & 31) & ((1u32 << width) - 1))
        }
        "v_bfe_i32" => {
            let width = c & 31;
            let kept = a >> (b & 31) & ((1u32 << width) - 1);
            Some(if width != 0 && kept >> (width - 1) & 1 != 0 {
                kept | u32::MAX << width
            } else {
                kept
            })
        }
        "v_bfi_b32" => Some(a & b | !a & c),
        "v_bcnt_u32_b32" => Some(a.count_ones().wrapping_add(b)),
        "v_ffbl_b32" => Some(if a == 0 { u32::MAX } else { a.trailing_zeros() }),
        "v_ffbh_u32" => Some(if a == 0 { u32::MAX } else { a.leading_zeros() }),
        "v_bfrev_b32" => Some(a.reverse_bits()),
        "v_perm_b32" => Some(eval::ternary(TernaryOp::PermuteBytes, a, b, c)),
        "v_med3_i32" => {
            let mut sorted = [a as i32, b as i32, c as i32];
            sorted.sort_unstable();
            Some(sorted[1] as u32)
        }
        "v_med3_u32" => {
            let mut sorted = [a, b, c];
            sorted.sort_unstable();
            Some(sorted[1])
        }
        "v_min3_u32" => Some(a.min(b).min(c)),
        "v_max3_u32" => Some(a.max(b).max(c)),
        "v_min3_f32" => Some(eval::binary(
            BinaryOp::FloatMin,
            eval::binary(BinaryOp::FloatMin, a, b),
            c,
        )),
        "v_max3_f32" => Some(eval::binary(
            BinaryOp::FloatMax,
            eval::binary(BinaryOp::FloatMax, a, b),
            c,
        )),
        _ => None,
    }
}
