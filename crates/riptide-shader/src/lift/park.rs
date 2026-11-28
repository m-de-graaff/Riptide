//! Scalar registers parked in the lanes of a vector register.
//!
//! A compiler that runs out of scalar registers keeps numbers in single
//! lanes of a vector register it has no other use for: `v_writelane_b32`
//! puts a scalar into lane k, and `v_readlane_b32` takes it out again
//! (section 7 of the research note). That looks across lanes only by its
//! spelling. The lane is a storage slot, and what goes in and comes out is
//! a number the whole wave shares.
//!
//! The lift writes the two instructions literally: the write as a select on
//! the lane's own bit of a constant mask, the read as a read of a lane by a
//! constant number. Both need the console's lane numbers, which a host
//! does not have. [`unpark`] finds the vector registers that are used for
//! nothing else and gives each parked lane a scalar variable of its own:
//! the write becomes a write of that variable and the read a read of it.
//!
//! This is as sound as the lift's treatment of scalar registers, and for
//! the same reason: every lane of a wave holds the same number in a scalar
//! register, so every lane writes the same number to the slot and reads the
//! same number back. A register that is touched in any other way (read as a
//! vector, written whole, read at a lane that is computed) is left as the
//! lift wrote it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::ir::{Function, Op, Terminator, Value, Var};

/// The lane a constant mask names, if it names exactly one.
fn single_lane(low: u32, high: u32) -> Option<u32> {
    match (low.count_ones(), high.count_ones()) {
        (1, 0) => Some(low.trailing_zeros()),
        (0, 1) => Some(32 + high.trailing_zeros()),
        _ => None,
    }
}

/// Turns vector registers that only park scalars into scalar variables.
/// Returns how many registers were turned.
pub(crate) fn unpark(function: &mut Function) -> usize {
    // Where each temporary is defined.
    let mut defined: HashMap<u32, (usize, usize)> = HashMap::new();
    for (block, code) in function.blocks.iter().enumerate() {
        for (index, inst) in code.insts.iter().enumerate() {
            for temp in inst.results() {
                defined.insert(temp.0, (block, index));
            }
        }
    }
    let op_at = |function: &Function, temp: u32| -> Option<(usize, usize)> {
        defined
            .get(&temp)
            .copied()
            .filter(|&(block, index)| function.blocks[block].insts.get(index).is_some())
    };
    // The vector register a temporary was read from, with where.
    let read_of = |function: &Function, value: Value| -> Option<(u8, usize, usize)> {
        let Value::Temp(temp) = value else {
            return None;
        };
        let (block, index) = op_at(function, temp.0)?;
        match function.blocks[block].insts[index].op {
            Op::Get(Var::Vector(register)) => Some((register, block, index)),
            _ => None,
        }
    };
    // Whether the register is written between two instructions of a block.
    let written_between =
        |function: &Function, register: u8, block: usize, from: usize, to: usize| {
            function.blocks[block].insts[from + 1..to].iter().any(
                |inst| matches!(inst.op, Op::Set(Var::Vector(target), _) if target == register),
            )
        };

    let mut bad: BTreeSet<u8> = BTreeSet::new();
    // The writes that park: where, the register, the lane and the number.
    let mut parks: Vec<(usize, usize, u8, u32, Value)> = Vec::new();
    // The reads of a parked lane: where, the register and the lane.
    let mut reads: Vec<(usize, usize, u8, u32)> = Vec::new();
    // The selects that belong to a parking write, by their temporary.
    let mut selects: HashMap<u32, u8> = HashMap::new();
    for (block, code) in function.blocks.iter().enumerate() {
        for (index, inst) in code.insts.iter().enumerate() {
            let Op::Set(Var::Vector(register), value) = inst.op else {
                continue;
            };
            // A parking write: the register keeps its value except in one
            // lane named by a constant.
            let parked = (|| {
                let Value::Temp(select) = value else {
                    return None;
                };
                let (at_block, at) = op_at(function, select.0)?;
                let Op::Select {
                    condition: Value::Temp(condition),
                    if_true,
                    if_false,
                } = function.blocks[at_block].insts[at].op
                else {
                    return None;
                };
                let (mask_block, mask_at) = op_at(function, condition.0)?;
                let Op::LaneBit {
                    low: Value::Word(low),
                    high: Value::Word(high),
                } = function.blocks[mask_block].insts[mask_at].op
                else {
                    return None;
                };
                let lane = single_lane(low, high)?;
                let (old, old_block, old_at) = read_of(function, if_false)?;
                let current = old == register
                    && old_block == block
                    && at_block == block
                    && old_at < index
                    && !written_between(function, register, block, old_at, index);
                current.then_some((select.0, lane, if_true))
            })();
            match parked {
                Some((select, lane, number)) => {
                    selects.insert(select, register);
                    parks.push((block, index, register, lane, number));
                }
                None => {
                    bad.insert(register);
                }
            }
        }
    }
    // Every other use of what was read from such a register, and of the
    // selects, rules the register out.
    for (block, code) in function.blocks.iter().enumerate() {
        for (index, inst) in code.insts.iter().enumerate() {
            let own = inst.results().next().map(|temp| temp.0);
            inst.op.for_each_operand(|value| {
                let Value::Temp(temp) = value else {
                    return;
                };
                if let Some(&register) = selects.get(&temp.0) {
                    let is_the_write = matches!(
                        inst.op,
                        Op::Set(Var::Vector(target), Value::Temp(written))
                            if target == register && written == temp
                    );
                    if !is_the_write {
                        bad.insert(register);
                    }
                }
                let Some((register, read_block, read_at)) = read_of(function, value) else {
                    return;
                };
                let allowed = match &inst.op {
                    Op::ReadLane {
                        value: read,
                        lane: Value::Word(_),
                    } => {
                        *read == value
                            && read_block == block
                            && !written_between(function, register, block, read_at, index)
                    }
                    Op::Select {
                        condition,
                        if_true,
                        if_false,
                    } => {
                        *if_false == value
                            && *condition != value
                            && *if_true != value
                            && own.is_some_and(|own| selects.get(&own) == Some(&register))
                    }
                    _ => false,
                };
                if !allowed {
                    bad.insert(register);
                }
            });
            if let Op::ReadLane {
                value,
                lane: Value::Word(lane),
            } = inst.op
                && let Some((register, _, _)) = read_of(function, value)
            {
                reads.push((block, index, register, lane));
            }
        }
        if let Terminator::Branch {
            condition: Value::Temp(temp),
            ..
        } = code.terminator
            && let Some(&register) = selects.get(&temp.0)
        {
            bad.insert(register);
        }
    }
    // A register is turned only if something is parked in it.
    let parked: BTreeSet<u8> = parks
        .iter()
        .map(|&(_, _, register, _, _)| register)
        .filter(|register| !bad.contains(register))
        .collect();
    if parked.is_empty() {
        return 0;
    }

    // A scalar variable for each parked lane, from the numbers no scalar
    // register of the program has.
    let taken: BTreeSet<u8> = function
        .variables()
        .into_iter()
        .filter_map(|var| match var {
            Var::Scalar(number) => Some(number),
            _ => None,
        })
        .collect();
    let mut free = (0..=u8::MAX).rev().filter(|number| !taken.contains(number));
    let mut slots: BTreeMap<(u8, u32), u8> = BTreeMap::new();
    let lanes = parks
        .iter()
        .map(|&(_, _, register, lane, _)| (register, lane))
        .chain(reads.iter().map(|&(_, _, register, lane)| (register, lane)));
    for key in lanes {
        if !parked.contains(&key.0) || slots.contains_key(&key) {
            continue;
        }
        let Some(number) = free.next() else {
            // Too many slots for the numbers there are: leave the program
            // as it was.
            return 0;
        };
        slots.insert(key, number);
    }
    for (block, index, register, lane, number) in parks {
        if let Some(&slot) = slots.get(&(register, lane)) {
            function.blocks[block].insts[index].op = Op::Set(Var::Scalar(slot), number);
        }
    }
    for (block, index, register, lane) in reads {
        if let Some(&slot) = slots.get(&(register, lane)) {
            function.blocks[block].insts[index].op = Op::Get(Var::Scalar(slot));
        }
    }
    parked.len()
}
