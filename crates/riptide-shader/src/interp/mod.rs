//! The reference interpreter: runs the intermediate form on the CPU.
//!
//! [`run_wave`] runs a [`Function`] for every lane of one wave. Everything
//! outside the program (inputs, title memory, textures, where exports go)
//! is supplied through the [`Host`] trait, so that a test can run the same
//! program here and somewhere else, on a GPU or in [`machine`], and compare
//! what came out.
//!
//! # Lanes and cross-lane operations
//!
//! The intermediate form is what one lane does, so the interpreter runs the
//! lanes one after another. A lane runs until it ends or reaches a
//! cross-lane operation, where it waits. When no lane can run, the waiting
//! lanes that are earliest in the program are resolved together: the
//! operation sees exactly those lanes. This is the meaning of "the lanes
//! that reach the operation together" in [`crate::ir`], and it is what a
//! host's wave intrinsics give.
//!
//! "Earliest" is decided as the console would order things: a lane inside a
//! loop is earlier than a lane after it, and of two lanes in the same loop
//! the one with fewer trips is earlier; otherwise by the order of the
//! blocks with back edges set aside. Lanes wait at the same place only if
//! they are in the same trip of every enclosing loop.
//!
//! Effects (stores, exports) happen lane by lane in that order, not
//! instruction by instruction across the wave. A program in which one lane
//! reads what another stored can therefore behave differently here than on
//! the console. Compiled shaders do that only through atomic operations and
//! the group-shared memory, so those ([`Op::is_ordered`]) are done the
//! console's way: the lanes wait at them as at a cross-lane operation and
//! do them together, in the order of their numbers, the earliest place in
//! the program first. The shared memory starts as zeroes and lasts for the
//! run: one wave is one thread group here.
//!
//! # The second interpreter
//!
//! [`machine`] interprets the machine instructions themselves, a whole wave
//! at a time, with the wave-level rules of the instruction set and none of
//! the lift's reasoning. Tests run synthetic programs through both and
//! compare, which is how the lift is shown to keep a program's meaning
//! without any game data.

pub mod machine;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use crate::ir::cfg::{ControlFlow, map_operands};
use crate::ir::eval::{self, BufferFormat, BufferView};
use crate::ir::{
    Address, AtomicPlace, BlockId, ExportTarget, Function, ImageKind, ImageOp, Input, Op,
    Terminator, Value, Var,
};

/// What a program touches outside itself.
pub trait Host {
    /// The value of a stage input for a lane. Inputs that the whole wave
    /// shares (user data, group numbers) are asked for with each lane and
    /// must not depend on it.
    fn input(&mut self, lane: usize, input: Input) -> u32;

    /// Reads a unit of `width` bits (8, 16 or 32) of title memory,
    /// zero-extended. Memory that is not mapped reads as zero.
    fn load(&mut self, address: u64, width: u8) -> u32;

    /// Writes the low `width` bits of `value` to title memory. A write to
    /// memory that is not mapped is dropped.
    fn store(&mut self, address: u64, width: u8, value: u32);

    /// Performs an image access. Every operand of `access` is a constant
    /// ([`Value::Word`] or [`Value::Flag`]): the lane's values, filled in.
    /// Returns the four results; they are ignored for a store.
    fn image(&mut self, lane: usize, access: &ImageOp) -> [u32; 4];

    /// Takes an export of a lane.
    fn export(&mut self, lane: usize, target: ExportTarget, values: [Option<u32>; 4]);

    /// Notes that a lane's pixel is discarded.
    fn discard(&mut self, lane: usize);
}

/// Why a run stopped early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpError {
    /// The lanes ran more steps together than the limit allows; the
    /// program probably loops forever on these inputs.
    StepLimit,
    /// A typed buffer access named a data format this crate does not know.
    UnknownFormat(u32),
    /// A typed buffer store in a format that cannot be written here.
    UnwritableFormat(u32),
    /// The function breaks a rule of the form. The lift's output does not.
    Invalid(&'static str),
    /// More lanes were asked for than a wave has.
    TooManyLanes,
}

impl std::fmt::Display for InterpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterpError::StepLimit => write!(f, "the program ran past the step limit"),
            InterpError::UnknownFormat(code) => write!(f, "unknown buffer format {code}"),
            InterpError::UnwritableFormat(code) => {
                write!(f, "buffer format {code} cannot be written")
            }
            InterpError::Invalid(what) => write!(f, "invalid intermediate form: {what}"),
            InterpError::TooManyLanes => write!(f, "a wave has at most 64 lanes"),
        }
    }
}

impl std::error::Error for InterpError {}

/// How many instructions all lanes of a wave may run together.
pub const STEP_LIMIT: u64 = 50_000_000;

/// One lane's progress.
struct Lane {
    /// The block the lane is in.
    block: BlockId,
    /// The next instruction of that block.
    at: usize,
    /// The temporaries, flags as 0 and 1.
    temps: Vec<u32>,
    /// The variables that were written.
    vars: HashMap<Var, u32>,
    /// The loops the lane is in, outermost first: header and trip number.
    nest: Vec<(BlockId, u64)>,
    /// Whether the lane has ended.
    done: bool,
}

impl Lane {
    fn value(&self, value: Value) -> u32 {
        match value {
            Value::Temp(temp) => self.temps[temp.0 as usize],
            Value::Word(word) => word,
            Value::Flag(flag) => u32::from(flag),
        }
    }

    fn var(&self, var: Var) -> u32 {
        match self.vars.get(&var) {
            Some(&value) => value,
            // Everything starts as zero but the live flag.
            None => u32::from(var == Var::Exec),
        }
    }
}

/// Runs `function` for lanes 0 to `lanes - 1` of one wave.
pub fn run_wave(function: &Function, lanes: usize, host: &mut dyn Host) -> Result<(), InterpError> {
    if lanes > 64 {
        return Err(InterpError::TooManyLanes);
    }
    if function.blocks.is_empty() {
        return Err(InterpError::Invalid("a function without blocks"));
    }
    let flow = ControlFlow::of(function);
    let mut state: Vec<Lane> = (0..lanes)
        .map(|_| Lane {
            block: 0,
            at: 0,
            temps: vec![0; function.temps as usize],
            vars: HashMap::new(),
            nest: Vec::new(),
            done: false,
        })
        .collect();
    let mut steps = 0u64;
    let mut shared = vec![0u8; function.interface.shared_bytes as usize];
    // The lanes that wait at a cross-lane operation.
    let mut waiting = vec![false; lanes];
    loop {
        for lane in 0..lanes {
            if !state[lane].done && !waiting[lane] {
                waiting[lane] =
                    run_lane(function, &flow, lane, &mut state[lane], host, &mut steps)?;
            }
        }
        // The earliest waiting lanes, and all that wait at the same place.
        let key = |lane: &Lane| -> Vec<(usize, u64)> {
            let place = |block: BlockId| flow.place[block].unwrap_or(usize::MAX);
            let mut key: Vec<(usize, u64)> = lane
                .nest
                .iter()
                .map(|&(header, trip)| (place(header), trip))
                .collect();
            key.push((place(lane.block), lane.at as u64));
            key
        };
        let Some(first) = (0..lanes)
            .filter(|&lane| waiting[lane])
            .map(|lane| key(&state[lane]))
            .min()
        else {
            break;
        };
        let group: Vec<usize> = (0..lanes)
            .filter(|&lane| waiting[lane] && key(&state[lane]) == first)
            .collect();
        resolve(function, &group, &mut state, host, &mut shared)?;
        for lane in group {
            waiting[lane] = false;
        }
    }
    Ok(())
}

/// Runs one lane until it ends (returns false) or waits at a cross-lane
/// operation (returns true).
fn run_lane(
    function: &Function,
    flow: &ControlFlow,
    index: usize,
    lane: &mut Lane,
    host: &mut dyn Host,
    steps: &mut u64,
) -> Result<bool, InterpError> {
    loop {
        let block = &function.blocks[lane.block];
        while let Some(inst) = block.insts.get(lane.at) {
            *steps += 1;
            if *steps > STEP_LIMIT {
                return Err(InterpError::StepLimit);
            }
            if inst.op.is_cross_lane() || inst.op.is_ordered() {
                return Ok(true);
            }
            let results = execute(function, index, lane, &inst.op, host)?;
            for (offset, value) in results.into_iter().enumerate() {
                lane.temps[inst.result.0 as usize + offset] = value;
            }
            lane.at += 1;
        }
        *steps += 1;
        if *steps > STEP_LIMIT {
            return Err(InterpError::StepLimit);
        }
        let next = match block.terminator {
            Terminator::Jump(to) => to,
            Terminator::Branch {
                condition,
                if_true,
                if_false,
            } => {
                if lane.value(condition) != 0 {
                    if_true
                } else {
                    if_false
                }
            }
            Terminator::Return => {
                lane.done = true;
                return Ok(false);
            }
        };
        // Keep track of the loops the lane is in and of its trips.
        while let Some(&(header, _)) = lane.nest.last() {
            let inside = flow
                .loops
                .iter()
                .any(|candidate| candidate.header == header && candidate.contains(next));
            if inside {
                break;
            }
            lane.nest.pop();
        }
        if flow.loops.iter().any(|candidate| candidate.header == next) {
            match lane.nest.last_mut() {
                Some((header, trip)) if *header == next => *trip += 1,
                _ => lane.nest.push((next, 0)),
            }
        }
        lane.block = next;
        lane.at = 0;
    }
}

/// Reads `count` bytes of title memory as one number, first byte lowest.
fn load_bytes(host: &mut dyn Host, address: u64, count: u32) -> u128 {
    let mut raw = 0u128;
    for index in 0..count {
        let byte = host.load(address.wrapping_add(u64::from(index)), 8) & 0xFF;
        raw |= u128::from(byte) << (8 * index);
    }
    raw
}

/// Executes one operation that does not look across lanes and returns its
/// results.
fn execute(
    function: &Function,
    index: usize,
    lane: &mut Lane,
    op: &Op,
    host: &mut dyn Host,
) -> Result<Vec<u32>, InterpError> {
    let v = |value: Value| lane.value(value);
    let address = |address: &Address| u64::from(v(address.low)) | u64::from(v(address.high)) << 32;
    let view = |descriptor: &[Value; 4]| {
        BufferView::from_words([
            v(descriptor[0]),
            v(descriptor[1]),
            v(descriptor[2]),
            v(descriptor[3]),
        ])
    };
    Ok(match op {
        Op::Get(var) => vec![lane.var(*var)],
        Op::Set(var, value) => {
            let value = v(*value);
            lane.vars.insert(*var, value);
            vec![]
        }
        Op::Unary(kind, a) => vec![eval::unary(*kind, v(*a))],
        Op::Binary(kind, a, b) => vec![eval::binary(*kind, v(*a), v(*b))],
        Op::Ternary(kind, a, b, c) => vec![eval::ternary(*kind, v(*a), v(*b), v(*c))],
        Op::Compare(kind, a, b) => vec![u32::from(eval::compare(*kind, v(*a), v(*b)))],
        Op::FloatClass { value, classes } => {
            vec![u32::from(eval::float_class(v(*value), v(*classes)))]
        }
        Op::Select {
            condition,
            if_true,
            if_false,
        } => vec![if v(*condition) != 0 {
            v(*if_true)
        } else {
            v(*if_false)
        }],
        Op::FlagNot(a) => vec![u32::from(v(*a) == 0)],
        Op::Flag(kind, a, b) => {
            let (a, b) = (v(*a) != 0, v(*b) != 0);
            vec![u32::from(match kind {
                crate::ir::FlagOp::And => a && b,
                crate::ir::FlagOp::Or => a || b,
                crate::ir::FlagOp::Xor => a != b,
            })]
        }
        Op::Carry { a, b, carry_in } => {
            vec![u32::from(eval::carry(v(*a), v(*b), v(*carry_in) != 0))]
        }
        Op::Borrow { a, b, borrow_in } => {
            vec![u32::from(eval::borrow(v(*a), v(*b), v(*borrow_in) != 0))]
        }
        Op::Input(input) => vec![host.input(index, *input)],
        Op::LoadMemory {
            address: at,
            width,
            words,
        } => {
            let base = address(at);
            (0..u64::from(*words))
                .map(|unit| host.load(base.wrapping_add(unit * u64::from(*width / 8)), *width))
                .collect()
        }
        Op::StoreMemory {
            address: at,
            width,
            values,
            guard,
        } => {
            if v(*guard) != 0 {
                let base = address(at);
                for (unit, value) in values.iter().enumerate() {
                    let at = base.wrapping_add(unit as u64 * u64::from(*width / 8));
                    host.store(at, *width, v(*value));
                }
            }
            vec![]
        }
        Op::LoadBuffer {
            descriptor,
            position,
            width,
            words,
        } => {
            let view = view(descriptor);
            let (element, offset) = (v(position.index), v(position.offset));
            (0..u32::from(*words))
                .map(|unit| {
                    let offset = offset.wrapping_add(unit * u32::from(*width / 8));
                    match view.locate(element, offset) {
                        Some(at) => host.load(at, *width),
                        None => 0,
                    }
                })
                .collect()
        }
        Op::StoreBuffer {
            descriptor,
            position,
            width,
            values,
            guard,
        } => {
            if v(*guard) != 0 {
                let view = view(descriptor);
                let (element, offset) = (v(position.index), v(position.offset));
                for (unit, value) in values.iter().enumerate() {
                    let offset = offset.wrapping_add(unit as u32 * u32::from(*width / 8));
                    if let Some(at) = view.locate(element, offset) {
                        host.store(at, *width, v(*value));
                    }
                }
            }
            vec![]
        }
        Op::LoadFormat {
            descriptor,
            position,
            format,
        } => {
            let code = v(*format);
            let format = BufferFormat::from_code(code).ok_or(InterpError::UnknownFormat(code))?;
            match view(descriptor).locate(v(position.index), v(position.offset)) {
                Some(at) => format.decode(load_bytes(host, at, format.bytes())).to_vec(),
                None => vec![0; 4],
            }
        }
        Op::StoreFormat {
            descriptor,
            position,
            format,
            values,
            guard,
        } => {
            let code = v(*format);
            let format = BufferFormat::from_code(code).ok_or(InterpError::UnknownFormat(code))?;
            if v(*guard) != 0
                && let Some(at) = view(descriptor).locate(v(position.index), v(position.offset))
            {
                let raw = format
                    .encode([v(values[0]), v(values[1]), v(values[2]), v(values[3])])
                    .ok_or(InterpError::UnwritableFormat(code))?;
                for byte in 0..format.bytes() {
                    let value = (raw >> (8 * byte)) as u32 & 0xFF;
                    host.store(at.wrapping_add(u64::from(byte)), 8, value);
                }
            }
            vec![]
        }
        Op::LoadCode { offset, words } => {
            let first = (v(*offset) / 4) as usize;
            (0..usize::from(*words))
                .map(|word| function.code.get(first + word).copied().unwrap_or(0))
                .collect()
        }
        Op::Image(image) => {
            // Hand the host the access with the lane's values filled in.
            let mut access: ImageOp = (**image).clone();
            let mut wrapped = Op::Image(Box::new(access.clone()));
            map_operands(&mut wrapped, |value| {
                *value = match *value {
                    Value::Flag(flag) => Value::Flag(flag),
                    other => Value::Word(lane.value(other)),
                };
            });
            if let Op::Image(filled) = wrapped {
                access = *filled;
            }
            // The guard of a store is a flag; put it back as one.
            let mut skip = false;
            if let (
                ImageKind::Store { guard, .. },
                ImageKind::Store {
                    guard: original, ..
                },
            ) = (&mut access.kind, &image.kind)
            {
                let live = lane.value(*original) != 0;
                *guard = Value::Flag(live);
                skip = !live;
            }
            let stores = matches!(access.kind, ImageKind::Store { .. });
            let results = if skip {
                [0; 4]
            } else {
                host.image(index, &access)
            };
            if stores { vec![] } else { results.to_vec() }
        }
        Op::Export {
            target,
            values,
            guard,
        } => {
            if v(*guard) != 0 {
                let values = values.map(|value| value.map(&v));
                host.export(index, *target, values);
            }
            vec![]
        }
        Op::Discard(condition) => {
            if v(*condition) != 0 {
                host.discard(index);
            }
            vec![]
        }
        Op::LaneBit { low, high } => {
            let bits = u64::from(v(*low)) | u64::from(v(*high)) << 32;
            vec![(bits >> index & 1) as u32]
        }
        Op::LaneIndex => vec![index as u32],
        Op::Atomic { .. } | Op::LoadShared { .. } | Op::StoreShared { .. } | Op::Barrier => {
            return Err(InterpError::Invalid(
                "an operation the lanes take in order run alone",
            ));
        }
        Op::Reduce { .. } | Op::PrefixSum { .. } => {
            return Err(InterpError::Invalid("a cross-lane operation run alone"));
        }
        Op::FirstLane { .. }
        | Op::ReadLane { .. }
        | Op::CountLanes(_)
        | Op::FirstLaneIndex(_)
        | Op::AnyLane(_)
        | Op::LanesBelow { .. }
        | Op::BallotWord { .. }
        | Op::QuadPermute { .. }
        | Op::QuadAny(_) => return Err(InterpError::Invalid("a cross-lane operation run alone")),
    })
}

/// One lane's turn at an operation the lanes take in order: an atomic
/// operation, an access to the shared memory, or a barrier.
fn take_turn(lane: &Lane, op: &Op, host: &mut dyn Host, shared: &mut [u8]) -> Vec<u32> {
    let v = |value: Value| lane.value(value);
    // The unit of `bytes` bytes at an offset of the shared memory, if all
    // of it is inside.
    let inside = |offset: u32, bytes: usize, size: usize| {
        let start = offset as usize;
        (start <= size && bytes <= size - start).then_some(start)
    };
    match op {
        Op::LoadShared {
            offset,
            width,
            words,
        } => {
            let bytes = usize::from(*width / 8);
            (0..u32::from(*words))
                .map(|unit| {
                    let offset = v(*offset).wrapping_add(unit * bytes as u32);
                    match inside(offset, bytes, shared.len()) {
                        Some(start) => shared[start..start + bytes]
                            .iter()
                            .rev()
                            .fold(0u32, |word, &byte| word << 8 | u32::from(byte)),
                        None => 0,
                    }
                })
                .collect()
        }
        Op::StoreShared {
            offset,
            width,
            values,
            guard,
        } => {
            let bytes = usize::from(*width / 8);
            if v(*guard) != 0 {
                for (unit, value) in values.iter().enumerate() {
                    let offset = v(*offset).wrapping_add((unit * bytes) as u32);
                    if let Some(start) = inside(offset, bytes, shared.len()) {
                        let word = v(*value).to_le_bytes();
                        shared[start..start + bytes].copy_from_slice(&word[..bytes]);
                    }
                }
            }
            vec![]
        }
        Op::Atomic {
            op,
            place,
            value,
            compare,
            guard,
        } => {
            if v(*guard) == 0 {
                return vec![0];
            }
            let (value, compare) = (v(*value), v(*compare));
            let at = match place {
                AtomicPlace::Memory(address) => {
                    Some(u64::from(v(address.low)) | u64::from(v(address.high)) << 32)
                }
                AtomicPlace::Buffer {
                    descriptor,
                    position,
                } => BufferView::from_words([
                    v(descriptor[0]),
                    v(descriptor[1]),
                    v(descriptor[2]),
                    v(descriptor[3]),
                ])
                .locate(v(position.index), v(position.offset)),
                AtomicPlace::Shared(offset) => {
                    let Some(start) = inside(v(*offset) & !3, 4, shared.len()) else {
                        return vec![0];
                    };
                    let mut word = [0u8; 4];
                    word.copy_from_slice(&shared[start..start + 4]);
                    let old = u32::from_le_bytes(word);
                    let new = eval::atomic(*op, old, value, compare);
                    shared[start..start + 4].copy_from_slice(&new.to_le_bytes());
                    return vec![old];
                }
            };
            match at {
                Some(at) => {
                    let at = at & !3;
                    let old = host.load(at, 32);
                    host.store(at, 32, eval::atomic(*op, old, value, compare));
                    vec![old]
                }
                None => vec![0],
            }
        }
        _ => vec![],
    }
}

/// Resolves the operation the lanes of `group` wait at, cross-lane or taken
/// in order, and moves them past it. `group` is sorted by lane number.
fn resolve(
    function: &Function,
    group: &[usize],
    state: &mut [Lane],
    host: &mut dyn Host,
    shared: &mut [u8],
) -> Result<(), InterpError> {
    let Some(&first) = group.first() else {
        return Ok(());
    };
    let inst = function.blocks[state[first].block]
        .insts
        .get(state[first].at)
        .ok_or(InterpError::Invalid("a lane waits past the end of a block"))?;
    if inst.op.is_ordered() {
        let result = inst.result.0 as usize;
        for &lane in group {
            let results = take_turn(&state[lane], &inst.op, host, shared);
            for (offset, value) in results.into_iter().enumerate() {
                state[lane].temps[result + offset] = value;
            }
            state[lane].at += 1;
        }
        return Ok(());
    }
    let value = |lane: usize, value: Value| state[lane].value(value);
    let holds = |lane: usize, flag: Value| state[lane].value(flag) != 0;
    let results: Vec<u32> = match &inst.op {
        Op::FirstLane {
            value: word,
            active,
        } => {
            let source = group
                .iter()
                .copied()
                .find(|&lane| holds(lane, *active))
                .unwrap_or(first);
            vec![value(source, *word); group.len()]
        }
        Op::ReadLane { value: word, lane } => group
            .iter()
            .map(|&reader| {
                let wanted = value(reader, *lane) as usize;
                if group.contains(&wanted) {
                    value(wanted, *word)
                } else {
                    0
                }
            })
            .collect(),
        Op::CountLanes(mask) => {
            let count = group.iter().filter(|&&lane| holds(lane, *mask)).count();
            vec![count as u32; group.len()]
        }
        Op::FirstLaneIndex(mask) => {
            let found = group.iter().copied().find(|&lane| holds(lane, *mask));
            vec![found.map_or(u32::MAX, |lane| lane as u32); group.len()]
        }
        Op::AnyLane(mask) => {
            let any = group.iter().any(|&lane| holds(lane, *mask));
            vec![u32::from(any); group.len()]
        }
        Op::LanesBelow { mask, half } => group
            .iter()
            .map(|&reader| {
                group
                    .iter()
                    .filter(|&&lane| {
                        lane < reader
                            && holds(lane, *mask)
                            && half.is_none_or(|high| (lane >= 32) == high)
                    })
                    .count() as u32
            })
            .collect(),
        Op::BallotWord { mask, high } => {
            let mut bits = 0u64;
            for &lane in group {
                if holds(lane, *mask) {
                    bits |= 1 << lane;
                }
            }
            let word = if *high {
                (bits >> 32) as u32
            } else {
                bits as u32
            };
            vec![word; group.len()]
        }
        Op::Reduce {
            op,
            value: word,
            active,
        } => {
            // Every lane of the console's wave, with the identity for one
            // that is not here or takes no part.
            let mut words = vec![op.identity(); function.wave.lanes()];
            for &lane in group {
                if holds(lane, *active) && lane < words.len() {
                    words[lane] = value(lane, *word);
                }
            }
            vec![eval::reduce(*op, &words); group.len()]
        }
        Op::PrefixSum {
            value: word,
            active,
        } => group
            .iter()
            .map(|&reader| {
                group
                    .iter()
                    .filter(|&&lane| lane < reader && holds(lane, *active))
                    .fold(0u32, |sum, &lane| sum.wrapping_add(value(lane, *word)))
            })
            .collect(),
        Op::QuadPermute {
            value: word,
            lanes,
            active,
        } => group
            .iter()
            .map(|&reader| {
                let source = (reader & !3) + usize::from(lanes[reader & 3]);
                if group.contains(&source) && holds(source, *active) {
                    value(source, *word)
                } else {
                    0
                }
            })
            .collect(),
        Op::QuadAny(mask) => group
            .iter()
            .map(|&reader| {
                let any = group
                    .iter()
                    .any(|&lane| lane & !3 == reader & !3 && holds(lane, *mask));
                u32::from(any)
            })
            .collect(),
        _ => {
            return Err(InterpError::Invalid(
                "a lane waits at an ordinary operation",
            ));
        }
    };
    let result = inst.result.0 as usize;
    for (&lane, value) in group.iter().zip(results) {
        state[lane].temps[result] = value;
        state[lane].at += 1;
    }
    Ok(())
}
