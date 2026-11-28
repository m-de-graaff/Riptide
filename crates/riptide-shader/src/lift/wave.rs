//! Lifting whole-wave sections: wave reductions and prefix sums.
//!
//! The console's compiler has no instruction for "the sum of a value over
//! the wave". It writes one out as a section of code that runs in every
//! lane, the ones that are not live included (section 7 of the research
//! note):
//!
//! - `s_or_saveexec x, -1` saves the live mask and makes every lane live;
//! - a `v_cndmask_b32` puts the value into the lanes that take part and the
//!   operation's identity into the others;
//! - a run of lane permutations (within eight lanes, within a row of
//!   sixteen, `v_permlanex16_b32` across rows) folds the lanes together,
//!   and `v_readlane_b32` takes the result out of one lane;
//! - a move of the saved mask back into `EXEC` ends the section.
//!
//! A host lane cannot run that code: it reads lanes by their console
//! numbers. So the lift does not translate the instructions of such a
//! section one by one. It runs the section on symbols, a lane of the
//! console's wave at a time, following for every lane of every register it
//! writes *which lanes' values were folded into it* ([`Sym`]). At the end a
//! register holds something the intermediate form can say, or it does not:
//!
//! - the fold of every lane, the same in each: [`Op::Reduce`];
//! - in lane n the sum of the lanes below n: [`Op::PrefixSum`] (with the
//!   lane's own value added, the sum up to and including n);
//! - the lane's own value, a constant, or a register from before the
//!   section;
//! - anything else, such as the fold of half a wave that a step on the way
//!   leaves behind. Compiled code does not read these again. Such a
//!   register is marked as left over, and the lift refuses a program that
//!   reads one before writing it.
//!
//! No sequence of instructions is matched, so the compiler's variants (32
//! and 64 lanes, reductions and prefix sums, a mask of the lanes that take
//! part) need no rules of their own; a section with a step the run on
//! symbols does not know is lifted like any other code, and then fails at
//! its first lane permutation.
//!
//! # What is exact
//!
//! A fold is only recognised where every lane's value went in exactly once
//! (or, for operations where twice is the same as once, at least once),
//! and a float sum only where the lanes were added as the balanced tree
//! [`ReduceOp::FloatAdd`] names. The identity in the lanes that take no
//! part has to be the operation's own.
//!
//! Taken on trust: what the permutations read (AMD's public description of
//! the instruction set), that `v_permlanex16_b32` fetches from lanes that
//! are not live when bit 0 of its select field is set and that bit 3 of
//! the field, which the title's compiler sets with it, changes nothing.

use std::collections::BTreeMap;

use super::{LEFT, Lift, Lifter, name_of};
use crate::gfx10::{
    Detail, Format, Half, Instruction, LanePermutation, Operand, Role, VectorModifiers,
};
use crate::ir::{BinaryOp, Op, ReduceOp, Value, Var, eval};

/// What one lane of a register holds inside a section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sym {
    /// Something the run on symbols does not follow.
    Unknown,
    /// A constant.
    Constant(u32),
    /// The lane's own word of a vector register, from before the section.
    Own(u8),
    /// The words of a leaf in the lanes of `lanes` (bit n for lane n),
    /// folded with the leaf's operation. One lane is the leaf itself.
    Fold {
        /// The leaf, by its place in `Section::leaves`.
        leaf: usize,
        /// The lanes folded in.
        lanes: u64,
    },
}

/// The per-lane value a fold starts from: `value` in the lanes that hold
/// `active`, the identity in the others.
struct Leaf {
    /// The lane's word.
    value: Value,
    /// Whether the lane takes part.
    active: Value,
    /// What the other lanes hold.
    identity: u32,
    /// What the leaf's lanes are folded with, once an instruction has said.
    op: Option<ReduceOp>,
}

/// What a register holds in every lane when a section ends.
enum Shape {
    /// What it held before.
    Unchanged,
    /// A constant.
    Constant(u32),
    /// A copy of another register from before the section.
    Copy(u8),
    /// A leaf: the lane's own value or the identity.
    Own(usize),
    /// The fold of a leaf over every lane.
    Reduced(usize),
    /// The sum of a leaf over the lanes below, and the lane's own if
    /// `inclusive`.
    Prefix { leaf: usize, inclusive: bool },
    /// Nothing the intermediate form can say.
    Left,
}

/// A section being run on symbols.
struct Section {
    /// How many lanes the wave has.
    lanes: usize,
    /// The live mask inside the section: every lane, until the section
    /// moves a constant into it.
    exec: u64,
    /// The vector registers written so far, a symbol per lane.
    vectors: BTreeMap<u8, Vec<Sym>>,
    /// The scalar registers `v_readlane_b32` wrote.
    scalars: BTreeMap<u8, Sym>,
    /// The scalar registers that hold the saved mask or a part of it.
    masks: Vec<u8>,
    leaves: Vec<Leaf>,
}

/// The reduction a vector operation is one step of.
fn reduce_op(name: &str) -> Option<ReduceOp> {
    Some(match name {
        "v_add_nc_u32" => ReduceOp::Add,
        "v_add_f32" => ReduceOp::FloatAdd,
        "v_and_b32" => ReduceOp::And,
        "v_or_b32" => ReduceOp::Or,
        "v_xor_b32" => ReduceOp::Xor,
        "v_min_u32" => ReduceOp::MinUnsigned,
        "v_max_u32" => ReduceOp::MaxUnsigned,
        "v_min_i32" => ReduceOp::MinSigned,
        "v_max_i32" => ReduceOp::MaxSigned,
        "v_min_f32" => ReduceOp::FloatMin,
        "v_max_f32" => ReduceOp::FloatMax,
        _ => return None,
    })
}

/// Whether two sets of lanes are the two halves of one aligned run: lanes
/// 2k and 2k+1, two such pairs side by side, and so on up to the halves of
/// the wave. Sums joined only this way make the balanced tree over the
/// lanes in their order.
fn halves_of_a_run(a: u64, b: u64) -> bool {
    let count = a.count_ones();
    if count != b.count_ones() || !count.is_power_of_two() || count > 32 {
        return false;
    }
    let run = |set: u64| {
        let first = set.trailing_zeros();
        (first.is_multiple_of(count) && set >> first == (1u64 << count) - 1).then_some(first)
    };
    match (run(a), run(b)) {
        (Some(x), Some(y)) => x.min(y).is_multiple_of(2 * count) && x.abs_diff(y) == count,
        _ => false,
    }
}

/// The lanes below `lane`.
fn below(lane: usize) -> u64 {
    (1u64 << lane) - 1
}

/// The lane that `lane` reads source 0 from through a permutation, whether
/// the read also takes a lane that is not live, and whether a read that
/// finds nothing gives zero (otherwise the lane is not written). A source
/// of `None` lies outside the row. `None` altogether is a permutation
/// without a rule here.
fn permuted_source(
    permutation: LanePermutation,
    lane: usize,
) -> Option<(Option<usize>, bool, bool)> {
    match permutation {
        LanePermutation::Eight {
            lanes,
            fetch_inactive,
        } => {
            let source = (lane & !7) + usize::from(lanes[lane & 7] & 7);
            Some((Some(source), fetch_inactive, false))
        }
        LanePermutation::Rows {
            control,
            fetch_inactive,
            bound_control,
            bank_mask,
            row_mask,
        } => {
            if bank_mask != 0xF || row_mask != 0xF {
                return None;
            }
            let (row, place) = (lane & !15, (lane & 15) as i32);
            let amount = i32::from(control & 15);
            let source = match control {
                0x000..=0x0FF => {
                    let quad = (control >> (2 * (lane & 3)) & 3) as i32;
                    Some((place & !3) + quad)
                }
                // After a shift right a lane holds what the lane that many
                // places below it held; after a shift left, the lane above.
                0x101..=0x10F => Some(place + amount),
                0x111..=0x11F => Some(place - amount),
                0x121..=0x12F => Some((place - amount).rem_euclid(16)),
                0x140 => Some(15 - place),
                0x141 => Some((place & 8) + 7 - (place & 7)),
                _ => return None,
            };
            let source = source
                .filter(|place| (0..16).contains(place))
                .map(|place| row + place as usize);
            Some((source, fetch_inactive, bound_control))
        }
    }
}

impl Section {
    fn all(&self) -> u64 {
        if self.lanes >= 64 {
            u64::MAX
        } else {
            below(self.lanes)
        }
    }

    /// What a lane of a vector register holds.
    fn vector(&self, register: u8, lane: usize) -> Sym {
        match self.vectors.get(&register) {
            Some(lanes) => lanes[lane],
            None => Sym::Own(register),
        }
    }

    /// What an operand is in a lane.
    fn value(&self, operand: Operand, lane: usize) -> Sym {
        match operand {
            Operand::Vector { first, count: 1 } => self.vector(first, lane),
            Operand::Scalar { first, count: 1 } => {
                self.scalars.get(&first).copied().unwrap_or(Sym::Unknown)
            }
            Operand::Integer(value) => Sym::Constant(value as i32 as u32),
            Operand::Literal(word) => Sym::Constant(word),
            Operand::Float(value) => Sym::Constant(value.value().to_bits()),
            _ => Sym::Unknown,
        }
    }

    /// Two symbols folded with `op`.
    fn combine(&mut self, op: ReduceOp, a: Sym, b: Sym) -> Sym {
        // Whether a leaf can be folded with this operation, which then
        // becomes the leaf's.
        let fits = |leaves: &mut Vec<Leaf>, leaf: usize| {
            let known = &mut leaves[leaf];
            if known.identity != op.identity() || known.op.is_some_and(|other| other != op) {
                return false;
            }
            known.op = Some(op);
            true
        };
        match (a, b) {
            (Sym::Constant(a), Sym::Constant(b)) => Sym::Constant(eval::binary(op.binary(), a, b)),
            (
                Sym::Fold { leaf, lanes },
                Sym::Fold {
                    leaf: other,
                    lanes: more,
                },
            ) if leaf == other => {
                let once = lanes & more == 0;
                let allowed = match op {
                    ReduceOp::FloatAdd => once && halves_of_a_run(lanes, more),
                    _ => once || op.idempotent(),
                };
                if allowed && fits(&mut self.leaves, leaf) {
                    Sym::Fold {
                        leaf,
                        lanes: lanes | more,
                    }
                } else {
                    Sym::Unknown
                }
            }
            // The identity changes nothing. (Not so for a float sum, where
            // adding +0 to -0 gives +0.)
            (Sym::Fold { leaf, lanes }, Sym::Constant(constant))
            | (Sym::Constant(constant), Sym::Fold { leaf, lanes })
                if constant == op.identity()
                    && op != ReduceOp::FloatAdd
                    && fits(&mut self.leaves, leaf) =>
            {
                Sym::Fold { leaf, lanes }
            }
            _ => Sym::Unknown,
        }
    }

    /// A sum less a part of it: how the compiler takes a lane's own value
    /// out of a prefix sum.
    fn remove(&mut self, a: Sym, b: Sym) -> Sym {
        match (a, b) {
            (
                Sym::Fold { leaf, lanes },
                Sym::Fold {
                    leaf: other,
                    lanes: less,
                },
            ) if leaf == other && lanes & less == less => {
                let known = &mut self.leaves[leaf];
                if known.identity != 0 || known.op.is_some_and(|op| op != ReduceOp::Add) {
                    return Sym::Unknown;
                }
                known.op = Some(ReduceOp::Add);
                if lanes == less {
                    Sym::Constant(0)
                } else {
                    Sym::Fold {
                        leaf,
                        lanes: lanes & !less,
                    }
                }
            }
            _ => Sym::Unknown,
        }
    }

    /// What a register holds in every lane, from its symbols.
    fn shape(&self, register: Option<u8>, symbols: &[Sym]) -> Shape {
        let all = self.all();
        let every = |test: &dyn Fn(usize, Sym) -> bool| {
            symbols
                .iter()
                .enumerate()
                .all(|(lane, &symbol)| test(lane, symbol))
        };
        let first = symbols[0];
        match first {
            Sym::Constant(constant) if every(&|_, symbol| symbol == first) => {
                return Shape::Constant(constant);
            }
            Sym::Own(source) if every(&|_, symbol| symbol == first) => {
                return if register == Some(source) {
                    Shape::Unchanged
                } else {
                    Shape::Copy(source)
                };
            }
            _ => {}
        }
        // Everything else is about one leaf.
        let Some(leaf) = symbols.iter().find_map(|symbol| match symbol {
            Sym::Fold { leaf, .. } => Some(*leaf),
            _ => None,
        }) else {
            return Shape::Left;
        };
        let folded = |symbol: Sym| match symbol {
            Sym::Fold { leaf: other, lanes } if other == leaf => Some(lanes),
            _ => None,
        };
        let op = self.leaves[leaf].op;
        if op.is_some() && every(&|_, symbol| folded(symbol) == Some(all)) {
            return Shape::Reduced(leaf);
        }
        if every(&|lane, symbol| folded(symbol) == Some(1 << lane)) {
            return Shape::Own(leaf);
        }
        if op == Some(ReduceOp::Add) {
            // Lane 0 has nothing below it: the sum there is zero.
            let exclusive = |lane: usize, symbol: Sym| {
                if lane == 0 {
                    symbol == Sym::Constant(0)
                } else {
                    folded(symbol) == Some(below(lane))
                }
            };
            if every(&exclusive) {
                return Shape::Prefix {
                    leaf,
                    inclusive: false,
                };
            }
            if every(&|lane, symbol| folded(symbol) == Some(below(lane) | 1 << lane)) {
                return Shape::Prefix {
                    leaf,
                    inclusive: true,
                };
            }
        }
        Shape::Left
    }
}

/// Whether an instruction looks at other lanes, which is what makes a
/// section worth running on symbols.
fn crosses_lanes(instruction: &Instruction) -> bool {
    let permuted = matches!(
        instruction.detail,
        Detail::Vector(VectorModifiers {
            permutation: Some(_),
            ..
        })
    );
    permuted
        || matches!(
            name_of(instruction),
            "v_permlanex16_b32" | "v_permlane16_b32"
        )
}

impl Lifter<'_> {
    /// If the instruction at `start` opens a whole-wave section that ends
    /// before `last` and can be run on symbols, lifts all of it and returns
    /// the index of the instruction after it. Otherwise lifts nothing.
    pub(super) fn wave_section(&mut self, start: usize, last: usize) -> Lift<Option<usize>> {
        let opening = &self.instructions[start];
        let words = self.mask_words();
        let opens = match name_of(opening) {
            "s_or_saveexec_b32" => words == 1,
            "s_or_saveexec_b64" => words == 2,
            _ => false,
        } && opening.sources().next() == Some(Operand::Integer(-1));
        let Some(Operand::Scalar {
            first: saved,
            count,
        }) = opening.arg(Role::Result)
        else {
            return Ok(None);
        };
        if !opens || count != words {
            return Ok(None);
        }
        // The section ends where the saved mask goes back into EXEC.
        let whole = if words == 2 { Half::Both } else { Half::Low };
        let restores = |instruction: &Instruction| {
            matches!(name_of(instruction), "s_mov_b32" | "s_mov_b64")
                && instruction.arg(Role::Result) == Some(Operand::Exec(whole))
                && instruction.sources().next()
                    == Some(Operand::Scalar {
                        first: saved,
                        count: words,
                    })
        };
        let Some(end) = (start + 1..last).find(|&index| restores(&self.instructions[index])) else {
            return Ok(None);
        };
        if !self.instructions[start + 1..end].iter().any(crosses_lanes) {
            return Ok(None);
        }
        // Whatever the attempt lifts is taken back if it does not get to
        // the end.
        let kept = (self.insts.len(), self.temps, self.state.clone(), self.at);
        match self.run_section(start, end, saved) {
            Ok(true) => Ok(Some(end + 1)),
            Ok(false) | Err(_) => {
                self.insts.truncate(kept.0);
                self.temps = kept.1;
                self.state = kept.2;
                self.at = kept.3;
                Ok(None)
            }
        }
    }

    /// Runs the section from `start` to its restore at `end` on symbols and
    /// lifts what it leaves. Returns false at a step it has no rule for.
    fn run_section(&mut self, start: usize, end: usize, saved: u8) -> Lift<bool> {
        let lanes = self.wave.lanes();
        let mut section = Section {
            lanes,
            exec: 0,
            vectors: BTreeMap::new(),
            scalars: BTreeMap::new(),
            masks: vec![saved],
            leaves: Vec::new(),
        };
        section.exec = section.all();
        let opening = self.instructions[start].clone();
        self.at = (Some(opening.offset), name_of(&opening));
        self.instruction(start, &opening)?;
        for index in start + 1..end {
            let instruction = self.instructions[index].clone();
            self.at = (Some(instruction.offset), name_of(&instruction));
            if !self.section_step(&mut section, index, &instruction)? {
                return Ok(false);
            }
        }
        self.leave_section(&section)?;
        let closing = self.instructions[end].clone();
        self.at = (Some(closing.offset), name_of(&closing));
        self.instruction(end, &closing)?;
        Ok(true)
    }

    /// One instruction of a section. Returns false if there is no rule for
    /// it there.
    fn section_step(
        &mut self,
        section: &mut Section,
        index: usize,
        instruction: &Instruction,
    ) -> Lift<bool> {
        let name = name_of(instruction);
        let words = self.mask_words();
        let sources: Vec<Operand> = instruction.sources().collect();
        let target = instruction.arg(Role::Result);
        match instruction.format {
            // Waits and hints.
            Format::Sopp => {
                self.instruction(index, instruction)?;
                return Ok(true);
            }
            Format::Sop1 | Format::Sop2 => {}
            Format::Vop1 | Format::Vop2 | Format::Vop3a => {
                return self.section_vector(section, index, instruction);
            }
            _ => return Ok(false),
        }
        let constant = |operand: Option<&Operand>| match operand {
            Some(&Operand::Integer(value)) => Some(value as i32 as u32),
            Some(&Operand::Literal(word)) => Some(word),
            _ => None,
        };
        match (name, target) {
            // A constant moved into the live mask: the compiler narrows the
            // section to some rows or to one half of the wave.
            ("s_mov_b32", Some(Operand::Exec(half))) => {
                let Some(bits) = constant(sources.first()) else {
                    return Ok(false);
                };
                match half {
                    Half::Low if words == 1 => section.exec = u64::from(bits),
                    Half::Low => {
                        section.exec = section.exec & !0xFFFF_FFFF | u64::from(bits);
                    }
                    Half::High if words == 2 => {
                        section.exec = section.exec & 0xFFFF_FFFF | u64::from(bits) << 32;
                    }
                    _ => return Ok(false),
                }
                Ok(true)
            }
            // The lanes that take part: a mask and the saved one.
            ("s_and_b32" | "s_and_b64", Some(Operand::Scalar { first, count }))
                if count == words && !section.masks.contains(&first) =>
            {
                let of_saved = sources.iter().any(|&source| {
                    matches!(source, Operand::Scalar { first: register, count }
                        if count == words && section.masks.contains(&register))
                });
                if !of_saved {
                    return Ok(false);
                }
                self.instruction(index, instruction)?;
                section.scalars.remove(&first);
                section.masks.push(first);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// One vector instruction of a section.
    fn section_vector(
        &mut self,
        section: &mut Section,
        index: usize,
        instruction: &Instruction,
    ) -> Lift<bool> {
        let name = name_of(instruction);
        if name == "v_nop" {
            return Ok(true);
        }
        let sources: Vec<Operand> = instruction.sources().collect();
        let modifiers = match instruction.detail {
            Detail::Vector(modifiers) => modifiers,
            _ => VectorModifiers::default(),
        };
        let lanes = section.lanes;
        if name == "v_readlane_b32" {
            let (Some(Operand::Scalar { first, count: 1 }), Some(&source), Some(&lane)) = (
                instruction.arg(Role::Result),
                sources.first(),
                sources.get(1),
            ) else {
                return Ok(false);
            };
            let lane = match lane {
                Operand::Integer(lane) if lane >= 0 && (lane as usize) < lanes => lane as usize,
                _ => return Ok(false),
            };
            if section.masks.first() == Some(&first)
                || usize::from(first) >= super::SCALARS - 2
                || !matches!(source, Operand::Vector { count: 1, .. })
            {
                return Ok(false);
            }
            // The result often goes where the mask of the lanes that take
            // part was, which is then no mask any more.
            section.masks.retain(|&mask| mask != first);
            let read = section.value(source, lane);
            if matches!(read, Sym::Own(_)) {
                // A lane of a register the section has not touched: a
                // scalar a compiler parked there, most likely. It is read
                // as anywhere else. (A copy the section made of such a
                // register is not: nothing has written it yet.)
                let untouched = matches!(source, Operand::Vector { first, .. }
                    if !section.vectors.contains_key(&first));
                if !untouched {
                    return Ok(false);
                }
                section.scalars.remove(&first);
                self.instruction(index, instruction)?;
                return Ok(true);
            }
            section.scalars.insert(first, read);
            return Ok(true);
        }
        let Some(Operand::Vector {
            first: target,
            count: 1,
        }) = instruction.arg(Role::Result)
        else {
            return Ok(false);
        };
        let Some(&first) = sources.first() else {
            return Ok(false);
        };
        let old: Vec<Sym> = (0..lanes)
            .map(|lane| section.vector(target, lane))
            .collect();
        let mut result = vec![Sym::Unknown; lanes];
        // The lanes the instruction writes.
        let mut writes = section.exec;

        // Source 0 as each lane fetches it.
        let across = name == "v_permlanex16_b32";
        let plain = VectorModifiers {
            permutation: None,
            select: 0,
            ..modifiers
        } == VectorModifiers::default();
        // Bit 0 of the select field of the row-crossing permutation is
        // "fetch lanes that are not live", bit 1 "a read that finds nothing
        // gives zero"; bit 3 comes with bit 0 in compiled code.
        let select_known = if across {
            modifiers.select & !0b1011 == 0
        } else {
            modifiers.select == 0
        };
        let mut fetched = vec![Sym::Unknown; lanes];
        let mut followed = plain && select_known;
        if across {
            let selector = |operand: Option<&Operand>| match operand {
                Some(&Operand::Integer(value)) => Some(value as i32 as u32),
                Some(&Operand::Literal(word)) => Some(word),
                _ => None,
            };
            match (selector(sources.get(1)), selector(sources.get(2))) {
                (Some(low), Some(high)) if followed => {
                    let picks = u64::from(low) | u64::from(high) << 32;
                    let fetch_inactive = modifiers.select & 1 != 0;
                    let bound_control = modifiers.select & 2 != 0;
                    for (lane, slot) in fetched.iter_mut().enumerate() {
                        // The other row of the lane's half of the wave.
                        let row = (lane & !31) | ((lane & 16) ^ 16);
                        let source = row | (picks >> (4 * (lane & 15)) & 15) as usize;
                        if fetch_inactive || section.exec >> source & 1 != 0 {
                            *slot = section.value(first, source);
                        } else if bound_control {
                            *slot = Sym::Constant(0);
                        } else {
                            writes &= !(1 << lane);
                        }
                    }
                }
                _ => followed = false,
            }
        } else if let Some(permutation) = modifiers.permutation {
            for (lane, slot) in fetched.iter_mut().enumerate() {
                let Some((source, fetch_inactive, bound_control)) =
                    permuted_source(permutation, lane)
                else {
                    followed = false;
                    break;
                };
                let found = source.filter(|&source| {
                    source < lanes && (fetch_inactive || section.exec >> source & 1 != 0)
                });
                match found {
                    Some(source) => *slot = section.value(first, source),
                    None if bound_control => *slot = Sym::Constant(0),
                    None => writes &= !(1 << lane),
                }
            }
        } else {
            for (lane, slot) in fetched.iter_mut().enumerate() {
                *slot = section.value(first, lane);
            }
        }

        if followed {
            match name {
                "v_mov_b32" | "v_permlanex16_b32" => result = fetched,
                "v_cndmask_b32" => {
                    // The identity where the mask is clear, the lane's own
                    // word of a register from before the section where it
                    // is set: a leaf.
                    let identity = fetched[0];
                    let alike = fetched.iter().all(|&symbol| symbol == identity);
                    let words = self.mask_words();
                    if let (
                        Sym::Constant(identity),
                        true,
                        Some(
                            &value @ Operand::Vector {
                                first: own,
                                count: 1,
                            },
                        ),
                        Some(
                            &mask @ Operand::Scalar {
                                first: register,
                                count,
                            },
                        ),
                    ) = (identity, alike, sources.get(1), sources.get(2))
                        && count == words
                        && section.masks.contains(&register)
                        && !section.vectors.contains_key(&own)
                    {
                        let value = self.word(value)?;
                        let active = self.mask(mask)?;
                        section.leaves.push(Leaf {
                            value,
                            active,
                            identity,
                            op: None,
                        });
                        let leaf = section.leaves.len() - 1;
                        for (lane, slot) in result.iter_mut().enumerate() {
                            *slot = Sym::Fold {
                                leaf,
                                lanes: 1 << lane,
                            };
                        }
                    }
                }
                "v_sub_nc_u32" | "v_subrev_nc_u32" => {
                    if let Some(&second) = sources.get(1) {
                        for (lane, slot) in result.iter_mut().enumerate() {
                            let (mut a, mut b) = (fetched[lane], section.value(second, lane));
                            if name == "v_subrev_nc_u32" {
                                std::mem::swap(&mut a, &mut b);
                            }
                            *slot = section.remove(a, b);
                        }
                    }
                }
                _ => {
                    if let (Some(op), Some(&second), 2) =
                        (reduce_op(name), sources.get(1), sources.len())
                    {
                        for (lane, slot) in result.iter_mut().enumerate() {
                            let other = section.value(second, lane);
                            *slot = section.combine(op, fetched[lane], other);
                        }
                    }
                }
            }
        }
        let merged = (0..lanes)
            .map(|lane| {
                if writes >> lane & 1 != 0 {
                    result[lane]
                } else {
                    old[lane]
                }
            })
            .collect();
        section.vectors.insert(target, merged);
        self.forget_weights(instruction);
        Ok(true)
    }

    /// Writes what a section leaves into the registers it wrote.
    fn leave_section(&mut self, section: &Section) -> Lift<()> {
        // What each leaf comes to, lifted once however many registers hold
        // it.
        let mut reduced: Vec<Option<Value>> = vec![None; section.leaves.len()];
        let mut prefixes: Vec<Option<Value>> = vec![None; section.leaves.len()];
        let mut owns: Vec<Option<Value>> = vec![None; section.leaves.len()];
        // Everything is read before anything is written.
        let mut values: Vec<(Option<u8>, Option<u8>, Option<Value>)> = Vec::new();
        let shapes: Vec<(Option<u8>, Option<u8>, Shape)> = section
            .vectors
            .iter()
            .map(|(&register, symbols)| {
                (Some(register), None, section.shape(Some(register), symbols))
            })
            .chain(section.scalars.iter().map(|(&register, &symbol)| {
                let shape = match section.shape(None, &vec![symbol; section.lanes]) {
                    // A scalar is one lane's word: only what is the same
                    // in every lane can be said of it.
                    shape @ (Shape::Constant(_) | Shape::Reduced(_)) => shape,
                    _ => Shape::Left,
                };
                (None, Some(register), shape)
            }))
            .collect();
        for (vector, scalar, shape) in shapes {
            let own = |lifter: &mut Self, owns: &mut Vec<Option<Value>>, leaf: usize| {
                *owns[leaf].get_or_insert_with(|| {
                    let known = &section.leaves[leaf];
                    lifter.select(known.active, known.value, Value::Word(known.identity))
                })
            };
            let value = match shape {
                Shape::Unchanged => continue,
                Shape::Left => None,
                Shape::Constant(constant) => Some(Value::Word(constant)),
                Shape::Copy(source) => Some(self.word(Operand::Vector {
                    first: source,
                    count: 1,
                })?),
                Shape::Own(leaf) => Some(own(self, &mut owns, leaf)),
                Shape::Reduced(leaf) => {
                    let known = &section.leaves[leaf];
                    let Some(op) = known.op else {
                        return self.fail("a whole-wave section the lift cannot follow");
                    };
                    Some(*reduced[leaf].get_or_insert_with(|| {
                        self.emit(Op::Reduce {
                            op,
                            value: known.value,
                            active: known.active,
                        })
                    }))
                }
                Shape::Prefix { leaf, inclusive } => {
                    let known = &section.leaves[leaf];
                    let sum = *prefixes[leaf].get_or_insert_with(|| {
                        self.emit(Op::PrefixSum {
                            value: known.value,
                            active: known.active,
                        })
                    });
                    Some(if inclusive {
                        let own = own(self, &mut owns, leaf);
                        self.binary(BinaryOp::Add, sum, own)
                    } else {
                        sum
                    })
                }
            };
            values.push((vector, scalar, value));
        }
        for (vector, scalar, value) in values {
            if let Some(register) = vector {
                let (word, bit) = (usize::from(register / 64), register % 64);
                match value {
                    // The section wrote every lane.
                    Some(value) => {
                        self.emit(Op::Set(Var::Vector(register), value));
                        self.state.left[word] &= !(1 << bit);
                    }
                    None => self.state.left[word] |= 1 << bit,
                }
            }
            if let Some(register) = scalar {
                match value {
                    Some(value) => self.set_number(register, value),
                    None => {
                        self.clobber(register);
                        self.state.forms[usize::from(register)] = LEFT;
                        self.state.constants[usize::from(register)] = None;
                    }
                }
            }
        }
        Ok(())
    }
}
