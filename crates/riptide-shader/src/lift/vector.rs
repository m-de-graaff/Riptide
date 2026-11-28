//! Lifting the vector formats: VOP1, VOP2, VOP3, VOPC, VOP3P and VINTRP.
//!
//! A vector operation computes one word per lane, so most of them lift to
//! one or a few operations of the intermediate form on the lane's own
//! words. What each computes is from AMD's public description of the
//! instruction set; the float rules that differ from a host's are in
//! [`crate::ir::eval`].

use super::{HIGH, Lift, Lifter, MASK, Rounding, name_of};
use crate::gfx10::{
    Detail, Format, Half, Instruction, LanePermutation, Operand, Role, VectorModifiers,
};
use crate::ir::{
    BinaryOp, CompareOp, FlagOp, Input, Op, TernaryOp, UnaryOp, Value, Var, VertexTerm,
};

/// The sign bit of a float.
const SIGN: u32 = 0x8000_0000;

impl Lifter<'_> {
    /// Lifts an instruction of a vector format.
    pub(super) fn vector(&mut self, index: usize, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        if matches!(name, "v_nop" | "v_pipeflush" | "v_clrexcp") {
            return Ok(());
        }
        if instruction.format == Format::Vintrp {
            return self.interpolate(index, instruction);
        }
        let modifiers = match instruction.detail {
            Detail::Vector(modifiers) => modifiers,
            _ => VectorModifiers::default(),
        };
        let sources: Vec<Operand> = instruction.sources().collect();
        let source = |lifter: &Self, index: usize| -> Lift<Operand> {
            match sources.get(index) {
                Some(&operand) => Ok(operand),
                None => lifter.fail("the instruction is not decoded"),
            }
        };
        let target = instruction.arg(Role::Result);
        let plain = modifiers == VectorModifiers::default();
        let sub_register = modifiers.sub_register;

        // The operations that are not "words in, a word out".
        match name {
            "v_readfirstlane_b32" => {
                let (register, _) = self.scalar_register(target)?;
                let value = self.word(source(self, 0)?)?;
                let active = self.live();
                let first = self.emit(Op::FirstLane { value, active });
                self.set_number(register, first);
                return Ok(());
            }
            "v_readlane_b32" => {
                let (register, _) = self.scalar_register(target)?;
                let value = self.word(source(self, 0)?)?;
                let lane = self.word(source(self, 1)?)?;
                let read = self.emit(Op::ReadLane { value, lane });
                self.set_number(register, read);
                return Ok(());
            }
            "v_writelane_b32" => {
                let (register, _) = self.vector_register(target)?;
                let value = self.word(source(self, 0)?)?;
                let Value::Word(lane) = self.word(source(self, 1)?)? else {
                    return self.fail("a write to a lane that is not a constant");
                };
                // The write goes to one lane of the wave whatever the live
                // mask says.
                let lane = lane % (32 * u32::from(self.mask_words()));
                let (low, high) = if lane < 32 {
                    (1u32 << lane, 0)
                } else {
                    (0, 1u32 << (lane - 32))
                };
                let this = self.emit(Op::LaneBit {
                    low: Value::Word(low),
                    high: Value::Word(high),
                });
                self.set_vector_if(register, value, this);
                return Ok(());
            }
            "v_cndmask_b32"
                if sub_register.is_none()
                    && modifiers.permutation.is_none()
                    && !modifiers.clamp
                    && modifiers.output_scale == 0 =>
            {
                let (register, _) = self.vector_register(target)?;
                let chosen = self.mask(source(self, 2)?)?;
                // The compiler folds the negation and the absolute value of
                // a float into the select that reads it.
                let mut picks = [self.word(source(self, 0)?)?, self.word(source(self, 1)?)?];
                for (index, value) in picks.iter_mut().enumerate() {
                    if modifiers.absolute >> index & 1 != 0 {
                        *value = self.binary(BinaryOp::And, *value, Value::Word(!SIGN));
                    }
                    if modifiers.negate >> index & 1 != 0 {
                        *value = self.binary(BinaryOp::Xor, *value, Value::Word(SIGN));
                    }
                }
                let value = self.select(chosen, picks[1], picks[0]);
                self.set_vector(register, value);
                return Ok(());
            }
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" | "v_add_co_ci_u32"
            | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32"
                if plain =>
            {
                return self.vector_carry(instruction, &sources);
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" if plain => {
                return self.lanes_below(instruction, &sources);
            }
            _ => {}
        }
        if name.starts_with("v_cmp") {
            return self.vector_compare(instruction, &sources, modifiers);
        }

        let (register, count) = self.vector_register(target)?;
        if count != 1 || sources.iter().any(|operand| wide(*operand)) {
            return self.fail("a 64-bit vector operation");
        }
        let float_sources = name.ends_with("_f32");
        let float_result = match name.strip_prefix("v_cvt_") {
            Some(rest) => rest.starts_with("f32_"),
            None => float_sources,
        };
        let mix = matches!(
            name,
            "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16"
        );
        if instruction.format == Format::Vop3p && !mix {
            return self.fail("a packed 16-bit vector operation");
        }
        if !mix && (modifiers.select != 0 || modifiers.select_high != 0) {
            return self.fail("a vector operation on the high halves of its operands");
        }

        // Read the sources, source 0 through its lane permutation if it has
        // one, and apply the absolute-value and negation modifiers.
        let mut values = Vec::with_capacity(3);
        let mut write = self.live();
        for (index, &operand) in sources.iter().enumerate() {
            let mut value = self.word(operand)?;
            if let Some(sub) = sub_register
                && index < 2
            {
                value = self.sub_select(
                    value,
                    sub.source_select[index],
                    sub.source_sign_extend[index],
                )?;
            }
            if index == 0
                && let Some(permutation) = modifiers.permutation
            {
                let (permuted, valid) = self.permute(value, permutation)?;
                value = permuted;
                write = self.flag(FlagOp::And, write, valid);
            }
            if mix {
                // Bit i of `select_high` says source i is a 16-bit float,
                // and bit i of `select` which half of the register it is
                // in; the absolute-value bits of a mix are in the field of
                // the high-half negations.
                if modifiers.select_high >> index & 1 != 0 {
                    if modifiers.select >> index & 1 != 0 {
                        value = self.binary(BinaryOp::ShiftRight, value, Value::Word(16));
                    }
                    let half = self.binary(BinaryOp::And, value, Value::Word(0xFFFF));
                    value = self.unary(UnaryOp::FloatFromHalf, half);
                }
                if modifiers.negate_high >> index & 1 != 0 {
                    value = self.binary(BinaryOp::And, value, Value::Word(!SIGN));
                }
            } else if modifiers.absolute >> index & 1 != 0 {
                if !float_sources {
                    return self.fail("an absolute value of an operand that is not a float");
                }
                value = self.binary(BinaryOp::And, value, Value::Word(!SIGN));
            }
            if modifiers.negate >> index & 1 != 0 {
                if !float_sources && !mix {
                    return self.fail("a negation of an operand that is not a float");
                }
                value = self.binary(BinaryOp::Xor, value, Value::Word(SIGN));
            }
            values.push(value);
        }
        // The two-source forms that add to their result read it as the
        // third source.
        if matches!(name, "v_mac_f32" | "v_fmac_f32") && values.len() == 2 {
            values.push(self.emit(Op::Get(Var::Vector(register))));
        }

        let Some(mut value) = self.vector_value(name, &values)? else {
            return self.fail("a vector operation the lift has no rule for");
        };

        if modifiers.output_scale != 0 {
            if !float_result {
                return self.fail("an output scale on a result that is not a float");
            }
            let scale = [1.0, 2.0, 4.0, 0.5][usize::from(modifiers.output_scale & 3)];
            value = self.binary(BinaryOp::FloatMul, value, Value::float(scale));
        }
        if modifiers.clamp {
            if !float_result {
                return self.fail("a clamp of a result that is not a float");
            }
            value = self.unary(UnaryOp::Saturate, value);
        }
        // A 16-bit result goes to one half of the register and leaves the
        // other half as it was (taken on trust: LLVM says so of this GPU
        // generation, and so does the research note).
        let half = match name {
            "v_cvt_f16_f32" | "v_fma_mixlo_f16" | "v_lshrrev_b16" | "v_lshlrev_b16" => Some(false),
            "v_fma_mixhi_f16" => Some(true),
            _ => None,
        };
        if let Some(sub) = sub_register {
            // The sub-register word says which part of the register the
            // result goes to and what becomes of the rest.
            let (shift, bits) = match sub.result_select {
                0..=3 => (8 * u32::from(sub.result_select), 8),
                4 | 5 => (16 * u32::from(sub.result_select - 4), 16),
                _ => (0, 32),
            };
            if bits == 32 {
                if half.is_some() {
                    return self.fail("a 16-bit result written to a whole register");
                }
            } else {
                let field = (1u32 << bits) - 1;
                let low = self.binary(BinaryOp::And, value, Value::Word(field));
                let placed = self.binary(BinaryOp::ShiftLeft, low, Value::Word(shift));
                value = match sub.result_unused {
                    0 => placed,
                    1 => {
                        // The bits above the part repeat its top bit; the
                        // bits below are zero.
                        let top = 32 - bits - shift;
                        let up = self.binary(BinaryOp::ShiftLeft, placed, Value::Word(top));
                        self.binary(BinaryOp::ShiftRightSigned, up, Value::Word(top))
                    }
                    2 => {
                        let old = self.emit(Op::Get(Var::Vector(register)));
                        let kept = self.binary(BinaryOp::And, old, Value::Word(!(field << shift)));
                        self.binary(BinaryOp::Or, kept, placed)
                    }
                    _ => return self.fail("a sub-register form the lift has no rule for"),
                };
            }
        } else if let Some(high) = half {
            let old = self.emit(Op::Get(Var::Vector(register)));
            let (keep, shift) = if high { (0xFFFF, 16) } else { (0xFFFF_0000, 0) };
            let kept = self.binary(BinaryOp::And, old, Value::Word(keep));
            let placed = self.binary(BinaryOp::ShiftLeft, value, Value::Word(shift));
            value = self.binary(BinaryOp::Or, kept, placed);
        }
        self.set_vector_if(register, value, write);
        Ok(())
    }

    /// Converts a float to a 16-bit float in the rounding mode the program
    /// has set for 16-bit floats.
    fn half_of(&mut self, value: Value) -> Lift<Value> {
        match self.state.half_rounding {
            Rounding::Nearest => Ok(self.unary(UnaryOp::HalfFromFloat, value)),
            Rounding::TowardZero => Ok(self.unary(UnaryOp::HalfFromFloatTowardZero, value)),
            Rounding::Unknown => {
                self.fail("the rounding mode differs between the paths that get here")
            }
        }
    }

    /// Takes the part of a source that a sub-register word selects: a byte
    /// (0 to 3), a 16-bit half (4, 5) or all of it (6).
    fn sub_select(&mut self, value: Value, select: u8, sign_extend: bool) -> Lift<Value> {
        let (shift, bits) = match select {
            0..=3 => (8 * u32::from(select), 8),
            4 | 5 => (16 * u32::from(select - 4), 16),
            6 => return Ok(value),
            _ => return self.fail("a sub-register form the lift has no rule for"),
        };
        if sign_extend {
            let up = self.binary(BinaryOp::ShiftLeft, value, Value::Word(32 - bits - shift));
            Ok(self.binary(BinaryOp::ShiftRightSigned, up, Value::Word(32 - bits)))
        } else {
            let down = self.binary(BinaryOp::ShiftRight, value, Value::Word(shift));
            Ok(self.binary(BinaryOp::And, down, Value::Word((1u32 << bits) - 1)))
        }
    }

    /// Reads source 0 through a lane permutation. Returns the value and
    /// whether the lane is written at all.
    fn permute(&mut self, value: Value, permutation: LanePermutation) -> Lift<(Value, Value)> {
        let LanePermutation::Rows {
            control,
            fetch_inactive,
            bound_control,
            bank_mask,
            row_mask,
        } = permutation
        else {
            return self.fail("a permutation within eight lanes");
        };
        if control > 0xFF {
            return self.fail("a lane permutation across quads");
        }
        if bank_mask != 0xF || row_mask != 0xF {
            return self.fail("a lane permutation that leaves out lanes");
        }
        let lanes = [
            (control & 3) as u8,
            (control >> 2 & 3) as u8,
            (control >> 4 & 3) as u8,
            (control >> 6 & 3) as u8,
        ];
        // With the fetch-inactive bit a lane that is not live is read all
        // the same: this is how a pixel program reads its quad's helper
        // lanes.
        let active = if fetch_inactive {
            Value::Flag(true)
        } else {
            self.live()
        };
        let permuted = self.emit(Op::QuadPermute {
            value,
            lanes,
            active,
        });
        if bound_control || fetch_inactive {
            // A lane whose source is not live gets zero.
            return Ok((permuted, Value::Flag(true)));
        }
        // Without bound control such a lane is not written. Whether the
        // source is live is its own live flag, permuted the same way.
        let live_word = self.number_of_flag(active);
        let source_live = self.emit(Op::QuadPermute {
            value: live_word,
            lanes,
            active: Value::Flag(true),
        });
        let valid = self.compare(CompareOp::NotEqual, source_live, Value::Word(0));
        Ok((permuted, valid))
    }

    /// The additions and subtractions with a carry out, and those that also
    /// take a carry in.
    fn vector_carry(&mut self, instruction: &Instruction, sources: &[Operand]) -> Lift<()> {
        let name = name_of(instruction);
        let (register, _) = self.vector_register(instruction.arg(Role::Result))?;
        let Some(carry_target) = instruction.arg(Role::SecondResult) else {
            return self.fail("the instruction is not decoded");
        };
        let (Some(&first), Some(&second)) = (sources.first(), sources.get(1)) else {
            return self.fail("the instruction is not decoded");
        };
        // The carry in is read before the carry out is written: they are
        // often the same register.
        let carry_in = match sources.get(2) {
            Some(&operand) => self.mask(operand)?,
            None => Value::Flag(false),
        };
        let mut a = self.word(first)?;
        let mut b = self.word(second)?;
        if name.contains("subrev") {
            std::mem::swap(&mut a, &mut b);
        }
        let subtract = name.contains("sub");
        let op = if subtract {
            BinaryOp::Sub
        } else {
            BinaryOp::Add
        };
        let one = self.number_of_flag(carry_in);
        let partial = self.binary(op, a, b);
        let value = self.binary(op, partial, one);
        let carry = self.emit(if subtract {
            Op::Borrow {
                a,
                b,
                borrow_in: carry_in,
            }
        } else {
            Op::Carry { a, b, carry_in }
        });
        let live = self.live();
        let carry = self.flag(FlagOp::And, carry, live);
        self.set_vector(register, value);
        self.set_mask(carry_target, carry)
    }

    /// `v_mbcnt_lo` and `v_mbcnt_hi`: how many lanes below this one a mask
    /// has set, plus a number.
    fn lanes_below(&mut self, instruction: &Instruction, sources: &[Operand]) -> Lift<()> {
        let high = name_of(instruction) == "v_mbcnt_hi_u32_b32";
        let (register, _) = self.vector_register(instruction.arg(Role::Result))?;
        let (Some(&operand), Some(&addend)) = (sources.first(), sources.get(1)) else {
            return self.fail("the instruction is not decoded");
        };
        let wide = self.mask_words() == 2;
        let wanted = if high { Half::High } else { Half::Low };
        let mask = match operand {
            Operand::Exec(half) if half == wanted => self.live(),
            Operand::Vcc(half) if half == wanted => self.emit(Op::Get(Var::Vcc)),
            Operand::Scalar { first, count: 1 } => {
                let index = self.check_scalar(first)?;
                let forms = self.state.forms[index];
                if !high && forms & MASK != 0 {
                    self.mask_of_register(first)
                } else if high && wide && forms & HIGH != 0 {
                    self.mask_of_register(first - 1)
                } else {
                    return self.fail("a count of the lanes below in a number");
                }
            }
            _ => return self.fail("a count of the lanes below in a number"),
        };
        if high && !wide {
            return self.fail("the high half of a 32-lane mask");
        }
        let below = self.emit(Op::LanesBelow {
            mask,
            half: wide.then_some(high),
        });
        let addend = self.word(addend)?;
        let value = self.binary(BinaryOp::Add, below, addend);
        self.set_vector(register, value);
        Ok(())
    }

    /// The vector comparisons: a flag per lane into a mask register, or into
    /// the live mask.
    fn vector_compare(
        &mut self,
        instruction: &Instruction,
        sources: &[Operand],
        modifiers: VectorModifiers,
    ) -> Lift<()> {
        let name = name_of(instruction);
        let into_live = name.starts_with("v_cmpx_");
        let rest = name
            .strip_prefix("v_cmpx_")
            .or(name.strip_prefix("v_cmp_"))
            .unwrap_or("");
        let Some((relation, ty)) = rest.rsplit_once('_') else {
            return self.fail("a comparison the lift has no rule for");
        };
        let (Some(&first), Some(&second)) = (sources.first(), sources.get(1)) else {
            return self.fail("the instruction is not decoded");
        };
        if modifiers.permutation.is_some() {
            return self.fail("a comparison with a lane permutation");
        }
        let float = ty == "f32";
        if !float && (modifiers.absolute != 0 || modifiers.negate != 0) {
            return self.fail("a negation of an operand that is not a float");
        }
        let flag = match ty {
            "f32" | "i32" | "u32" => {
                if wide(first) || wide(second) {
                    return self.fail("a 64-bit vector operation");
                }
                let mut a = self.word(first)?;
                let mut b = self.word(second)?;
                if let Some(sub) = modifiers.sub_register {
                    a = self.sub_select(a, sub.source_select[0], sub.source_sign_extend[0])?;
                    b = self.sub_select(b, sub.source_select[1], sub.source_sign_extend[1])?;
                }
                for (index, value) in [&mut a, &mut b].into_iter().enumerate() {
                    if modifiers.absolute >> index & 1 != 0 {
                        *value = self.binary(BinaryOp::And, *value, Value::Word(!SIGN));
                    }
                    if modifiers.negate >> index & 1 != 0 {
                        *value = self.binary(BinaryOp::Xor, *value, Value::Word(SIGN));
                    }
                }
                if relation == "class" {
                    self.emit(Op::FloatClass {
                        value: a,
                        classes: b,
                    })
                } else {
                    let op = match (ty, relation) {
                        (_, "f") => None,
                        (_, "t" | "tru") => None,
                        ("f32", "lt") => Some(CompareOp::FloatLess),
                        ("f32", "eq") => Some(CompareOp::FloatEqual),
                        ("f32", "le") => Some(CompareOp::FloatLessEqual),
                        ("f32", "gt") => Some(CompareOp::FloatGreater),
                        ("f32", "lg") => Some(CompareOp::FloatLessOrGreater),
                        ("f32", "ge") => Some(CompareOp::FloatGreaterEqual),
                        ("f32", "o") => Some(CompareOp::FloatOrdered),
                        ("f32", "u") => Some(CompareOp::FloatUnordered),
                        ("f32", "nge") => Some(CompareOp::FloatNotGreaterEqual),
                        ("f32", "nlg") => Some(CompareOp::FloatNotLessOrGreater),
                        ("f32", "ngt") => Some(CompareOp::FloatNotGreater),
                        ("f32", "nle") => Some(CompareOp::FloatNotLessEqual),
                        ("f32", "neq") => Some(CompareOp::FloatNotEqual),
                        ("f32", "nlt") => Some(CompareOp::FloatNotLess),
                        ("i32" | "u32", "eq") => Some(CompareOp::Equal),
                        ("i32" | "u32", "ne") => Some(CompareOp::NotEqual),
                        ("i32", "lt") => Some(CompareOp::SignedLess),
                        ("i32", "le") => Some(CompareOp::SignedLessEqual),
                        ("i32", "gt") => Some(CompareOp::SignedGreater),
                        ("i32", "ge") => Some(CompareOp::SignedGreaterEqual),
                        ("u32", "lt") => Some(CompareOp::UnsignedLess),
                        ("u32", "le") => Some(CompareOp::UnsignedLessEqual),
                        ("u32", "gt") => Some(CompareOp::UnsignedGreater),
                        ("u32", "ge") => Some(CompareOp::UnsignedGreaterEqual),
                        _ => return self.fail("a comparison the lift has no rule for"),
                    };
                    match op {
                        Some(op) => self.compare(op, a, b),
                        None => Value::Flag(relation != "f"),
                    }
                }
            }
            "u64" | "i64"
                if matches!(relation, "eq" | "ne") && modifiers.sub_register.is_none() =>
            {
                let low_a = self.word_at(first, 0)?;
                let high_a = self.word_at(first, 1)?;
                let low_b = self.word_at(second, 0)?;
                let high_b = self.word_at(second, 1)?;
                let low = self.compare(CompareOp::Equal, low_a, low_b);
                let high = self.compare(CompareOp::Equal, high_a, high_b);
                let equal = self.flag(FlagOp::And, low, high);
                if relation == "ne" {
                    self.not(equal)
                } else {
                    equal
                }
            }
            _ => return self.fail("a comparison of 16-bit or 64-bit values"),
        };
        // A lane that is not live gets a clear bit.
        let live = self.live();
        let flag = self.flag(FlagOp::And, flag, live);
        let target = if into_live {
            Operand::Exec(if self.mask_words() == 2 {
                Half::Both
            } else {
                Half::Low
            })
        } else {
            match instruction.arg(Role::Result) {
                Some(target) => target,
                None => return self.fail("the instruction is not decoded"),
            }
        };
        self.set_mask(target, flag)
    }

    /// The value of a "words in, a word out" operation, or `None` if there
    /// is no rule for it. `s` holds the sources in order.
    fn vector_value(&mut self, name: &'static str, s: &[Value]) -> Lift<Option<Value>> {
        let need = |lifter: &Self, count: usize| -> Lift<()> {
            if s.len() >= count {
                Ok(())
            } else {
                lifter.fail("the instruction is not decoded")
            }
        };
        let unary = |lifter: &mut Self, op: UnaryOp| -> Lift<Option<Value>> {
            need(lifter, 1)?;
            Ok(Some(lifter.unary(op, s[0])))
        };
        let binary = |lifter: &mut Self, op: BinaryOp| -> Lift<Option<Value>> {
            need(lifter, 2)?;
            Ok(Some(lifter.binary(op, s[0], s[1])))
        };
        // The operations whose first operand is the shift count or the
        // value subtracted.
        let reversed = |lifter: &mut Self, op: BinaryOp| -> Lift<Option<Value>> {
            need(lifter, 2)?;
            Ok(Some(lifter.binary(op, s[1], s[0])))
        };
        let ternary = |lifter: &mut Self, op: TernaryOp| -> Lift<Option<Value>> {
            need(lifter, 3)?;
            Ok(Some(lifter.emit(Op::Ternary(op, s[0], s[1], s[2]))))
        };
        // Three operands folded with one operation.
        let three = |lifter: &mut Self, op: BinaryOp| -> Lift<Option<Value>> {
            need(lifter, 3)?;
            let first = lifter.binary(op, s[0], s[1]);
            Ok(Some(lifter.binary(op, first, s[2])))
        };
        // One operation on the first two operands, another with the third.
        let chain = |lifter: &mut Self, first: BinaryOp, second: BinaryOp| -> Lift<Option<Value>> {
            need(lifter, 3)?;
            let inner = lifter.binary(first, s[0], s[1]);
            Ok(Some(lifter.binary(second, inner, s[2])))
        };
        match name {
            "v_mov_b32" => {
                need(self, 1)?;
                Ok(Some(s[0]))
            }
            // Floats.
            "v_add_f32" => binary(self, BinaryOp::FloatAdd),
            "v_sub_f32" => binary(self, BinaryOp::FloatSub),
            "v_subrev_f32" => reversed(self, BinaryOp::FloatSub),
            "v_mul_f32" => binary(self, BinaryOp::FloatMul),
            "v_mul_legacy_f32" => binary(self, BinaryOp::FloatMulLegacy),
            "v_min_f32" => binary(self, BinaryOp::FloatMin),
            "v_max_f32" => binary(self, BinaryOp::FloatMax),
            "v_ldexp_f32" => binary(self, BinaryOp::FloatScale),
            "v_mac_f32" | "v_mad_f32" | "v_madak_f32" | "v_madmk_f32" => {
                ternary(self, TernaryOp::FloatMulAdd)
            }
            "v_fmac_f32" | "v_fma_f32" | "v_fmaak_f32" | "v_fmamk_f32" | "v_fma_mix_f32" => {
                ternary(self, TernaryOp::FloatFused)
            }
            "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                need(self, 3)?;
                let fused = self.emit(Op::Ternary(TernaryOp::FloatFused, s[0], s[1], s[2]));
                Ok(Some(self.half_of(fused)?))
            }
            "v_mad_legacy_f32" => chain(self, BinaryOp::FloatMulLegacy, BinaryOp::FloatAdd),
            "v_min3_f32" => three(self, BinaryOp::FloatMin),
            "v_max3_f32" => three(self, BinaryOp::FloatMax),
            "v_med3_f32" => ternary(self, TernaryOp::FloatMedian),
            "v_rcp_f32" | "v_rcp_iflag_f32" => unary(self, UnaryOp::Reciprocal),
            "v_rsq_f32" => unary(self, UnaryOp::ReciprocalSqrt),
            "v_sqrt_f32" => unary(self, UnaryOp::Sqrt),
            "v_exp_f32" => unary(self, UnaryOp::Exp2),
            "v_log_f32" => unary(self, UnaryOp::Log2),
            "v_sin_f32" => unary(self, UnaryOp::SinTurns),
            "v_cos_f32" => unary(self, UnaryOp::CosTurns),
            "v_floor_f32" => unary(self, UnaryOp::Floor),
            "v_ceil_f32" => unary(self, UnaryOp::Ceil),
            "v_trunc_f32" => unary(self, UnaryOp::Trunc),
            "v_rndne_f32" => unary(self, UnaryOp::RoundEven),
            "v_fract_f32" => unary(self, UnaryOp::Fract),
            "v_cubeid_f32" => ternary(self, TernaryOp::CubeFace),
            "v_cubesc_f32" => ternary(self, TernaryOp::CubeS),
            "v_cubetc_f32" => ternary(self, TernaryOp::CubeT),
            "v_cubema_f32" => ternary(self, TernaryOp::CubeMajor),
            // Conversions.
            "v_cvt_f32_i32" => unary(self, UnaryOp::FloatFromSigned),
            "v_cvt_f32_u32" => unary(self, UnaryOp::FloatFromUnsigned),
            "v_cvt_i32_f32" => unary(self, UnaryOp::SignedFromFloat),
            "v_cvt_u32_f32" => unary(self, UnaryOp::UnsignedFromFloat),
            "v_cvt_f16_f32" => {
                need(self, 1)?;
                Ok(Some(self.half_of(s[0])?))
            }
            "v_cvt_f32_f16" => {
                need(self, 1)?;
                let half = self.binary(BinaryOp::And, s[0], Value::Word(0xFFFF));
                Ok(Some(self.unary(UnaryOp::FloatFromHalf, half)))
            }
            "v_cvt_f32_ubyte0" | "v_cvt_f32_ubyte1" | "v_cvt_f32_ubyte2" | "v_cvt_f32_ubyte3" => {
                need(self, 1)?;
                let byte = u32::from(name.as_bytes()[name.len() - 1] - b'0');
                let shifted = self.binary(BinaryOp::ShiftRight, s[0], Value::Word(8 * byte));
                let byte = self.binary(BinaryOp::And, shifted, Value::Word(0xFF));
                Ok(Some(self.unary(UnaryOp::FloatFromUnsigned, byte)))
            }
            "v_cvt_rpi_i32_f32" | "v_cvt_flr_i32_f32" => {
                need(self, 1)?;
                let value = if name == "v_cvt_rpi_i32_f32" {
                    self.binary(BinaryOp::FloatAdd, s[0], Value::float(0.5))
                } else {
                    s[0]
                };
                let floor = self.unary(UnaryOp::Floor, value);
                Ok(Some(self.unary(UnaryOp::SignedFromFloat, floor)))
            }
            "v_cvt_pkrtz_f16_f32" => {
                need(self, 2)?;
                let low = self.unary(UnaryOp::HalfFromFloatTowardZero, s[0]);
                let high = self.unary(UnaryOp::HalfFromFloatTowardZero, s[1]);
                let high = self.binary(BinaryOp::ShiftLeft, high, Value::Word(16));
                Ok(Some(self.binary(BinaryOp::Or, low, high)))
            }
            "v_pack_b32_f16" => {
                need(self, 2)?;
                let low = self.binary(BinaryOp::And, s[0], Value::Word(0xFFFF));
                let high = self.binary(BinaryOp::ShiftLeft, s[1], Value::Word(16));
                Ok(Some(self.binary(BinaryOp::Or, low, high)))
            }
            // Integers.
            "v_add_nc_u32" => binary(self, BinaryOp::Add),
            "v_sub_nc_u32" => binary(self, BinaryOp::Sub),
            "v_subrev_nc_u32" => reversed(self, BinaryOp::Sub),
            "v_mul_lo_u32" | "v_mul_lo_i32" => binary(self, BinaryOp::Mul),
            "v_mul_hi_u32" => binary(self, BinaryOp::MulHighUnsigned),
            "v_mul_hi_i32" => binary(self, BinaryOp::MulHighSigned),
            "v_mul_u32_u24" | "v_mul_hi_u32_u24" | "v_mad_u32_u24" => {
                need(self, 2)?;
                let a = self.binary(BinaryOp::And, s[0], Value::Word(0xFF_FFFF));
                let b = self.binary(BinaryOp::And, s[1], Value::Word(0xFF_FFFF));
                Ok(Some(match name {
                    "v_mul_u32_u24" => self.binary(BinaryOp::Mul, a, b),
                    "v_mul_hi_u32_u24" => self.binary(BinaryOp::MulHighUnsigned, a, b),
                    _ => {
                        need(self, 3)?;
                        let product = self.binary(BinaryOp::Mul, a, b);
                        self.binary(BinaryOp::Add, product, s[2])
                    }
                }))
            }
            "v_mul_i32_i24" | "v_mad_i32_i24" => {
                need(self, 2)?;
                let mut signed = [s[0], s[1]];
                for value in &mut signed {
                    let up = self.binary(BinaryOp::ShiftLeft, *value, Value::Word(8));
                    *value = self.binary(BinaryOp::ShiftRightSigned, up, Value::Word(8));
                }
                let product = self.binary(BinaryOp::Mul, signed[0], signed[1]);
                Ok(Some(if name == "v_mad_i32_i24" {
                    need(self, 3)?;
                    self.binary(BinaryOp::Add, product, s[2])
                } else {
                    product
                }))
            }
            "v_and_b32" => binary(self, BinaryOp::And),
            "v_or_b32" => binary(self, BinaryOp::Or),
            "v_xor_b32" => binary(self, BinaryOp::Xor),
            "v_xnor_b32" => {
                need(self, 2)?;
                let differ = self.binary(BinaryOp::Xor, s[0], s[1]);
                Ok(Some(self.unary(UnaryOp::Not, differ)))
            }
            "v_not_b32" => unary(self, UnaryOp::Not),
            "v_lshrrev_b16" | "v_lshlrev_b16" => {
                need(self, 2)?;
                // The low 16 bits of the second operand, shifted by the low
                // 4 bits of the first; the result is 16 bits.
                let count = self.binary(BinaryOp::And, s[0], Value::Word(15));
                let value = self.binary(BinaryOp::And, s[1], Value::Word(0xFFFF));
                let shifted = self.binary(
                    if name == "v_lshrrev_b16" {
                        BinaryOp::ShiftRight
                    } else {
                        BinaryOp::ShiftLeft
                    },
                    value,
                    count,
                );
                Ok(Some(self.binary(
                    BinaryOp::And,
                    shifted,
                    Value::Word(0xFFFF),
                )))
            }
            "v_lshlrev_b32" => reversed(self, BinaryOp::ShiftLeft),
            "v_lshrrev_b32" => reversed(self, BinaryOp::ShiftRight),
            "v_ashrrev_i32" => reversed(self, BinaryOp::ShiftRightSigned),
            "v_min_u32" => binary(self, BinaryOp::MinUnsigned),
            "v_max_u32" => binary(self, BinaryOp::MaxUnsigned),
            "v_min_i32" => binary(self, BinaryOp::MinSigned),
            "v_max_i32" => binary(self, BinaryOp::MaxSigned),
            "v_min3_u32" => three(self, BinaryOp::MinUnsigned),
            "v_max3_u32" => three(self, BinaryOp::MaxUnsigned),
            "v_min3_i32" => three(self, BinaryOp::MinSigned),
            "v_max3_i32" => three(self, BinaryOp::MaxSigned),
            "v_med3_i32" | "v_med3_u32" => {
                need(self, 3)?;
                let (min, max) = if name == "v_med3_i32" {
                    (BinaryOp::MinSigned, BinaryOp::MaxSigned)
                } else {
                    (BinaryOp::MinUnsigned, BinaryOp::MaxUnsigned)
                };
                // The median is the larger of the smaller pair and the
                // smaller of the larger pair with the third.
                let low = self.binary(min, s[0], s[1]);
                let high = self.binary(max, s[0], s[1]);
                let capped = self.binary(min, high, s[2]);
                Ok(Some(self.binary(max, low, capped)))
            }
            "v_add3_u32" => three(self, BinaryOp::Add),
            "v_or3_b32" => three(self, BinaryOp::Or),
            "v_and_or_b32" => chain(self, BinaryOp::And, BinaryOp::Or),
            "v_xad_u32" => chain(self, BinaryOp::Xor, BinaryOp::Add),
            "v_lshl_or_b32" => chain(self, BinaryOp::ShiftLeft, BinaryOp::Or),
            "v_lshl_add_u32" => {
                need(self, 3)?;
                // The shift count is 3 bits here.
                let count = self.binary(BinaryOp::And, s[1], Value::Word(7));
                let shifted = self.binary(BinaryOp::ShiftLeft, s[0], count);
                Ok(Some(self.binary(BinaryOp::Add, shifted, s[2])))
            }
            "v_add_lshl_u32" => {
                need(self, 3)?;
                let sum = self.binary(BinaryOp::Add, s[0], s[1]);
                let count = self.binary(BinaryOp::And, s[2], Value::Word(7));
                Ok(Some(self.binary(BinaryOp::ShiftLeft, sum, count)))
            }
            "v_bfe_u32" | "v_bfe_i32" => {
                need(self, 3)?;
                let offset = self.binary(BinaryOp::And, s[1], Value::Word(31));
                let width = self.binary(BinaryOp::And, s[2], Value::Word(31));
                Ok(Some(self.extract(s[0], offset, width, name == "v_bfe_i32")))
            }
            "v_bfi_b32" => {
                need(self, 3)?;
                // Bits of the second operand where the first is set, of the
                // third where it is clear.
                let chosen = self.binary(BinaryOp::And, s[0], s[1]);
                let inverse = self.unary(UnaryOp::Not, s[0]);
                let rest = self.binary(BinaryOp::And, inverse, s[2]);
                Ok(Some(self.binary(BinaryOp::Or, chosen, rest)))
            }
            "v_bfm_b32" => {
                need(self, 2)?;
                let one = self.binary(BinaryOp::ShiftLeft, Value::Word(1), s[0]);
                let ones = self.binary(BinaryOp::Sub, one, Value::Word(1));
                Ok(Some(self.binary(BinaryOp::ShiftLeft, ones, s[1])))
            }
            "v_alignbit_b32" => {
                need(self, 3)?;
                // The 64-bit value with the first operand as its high half,
                // shifted right; the low 32 bits.
                let count = self.binary(BinaryOp::And, s[2], Value::Word(31));
                let none = self.compare(CompareOp::Equal, count, Value::Word(0));
                let inverse = self.binary(BinaryOp::Sub, Value::Word(32), count);
                let low = self.binary(BinaryOp::ShiftRight, s[1], count);
                let high = self.binary(BinaryOp::ShiftLeft, s[0], inverse);
                let high = self.select(none, Value::Word(0), high);
                Ok(Some(self.binary(BinaryOp::Or, low, high)))
            }
            "v_bcnt_u32_b32" => {
                need(self, 2)?;
                let count = self.unary(UnaryOp::CountBits, s[0]);
                Ok(Some(self.binary(BinaryOp::Add, count, s[1])))
            }
            "v_ffbl_b32" => unary(self, UnaryOp::FindLowBit),
            "v_ffbh_u32" => unary(self, UnaryOp::LeadingZeros),
            "v_ffbh_i32" => unary(self, UnaryOp::LeadingSignBits),
            "v_bfrev_b32" => unary(self, UnaryOp::ReverseBits),
            "v_perm_b32" => ternary(self, TernaryOp::PermuteBytes),
            _ => Ok(None),
        }
    }

    /// Lifts an attribute interpolation of a pixel program.
    fn interpolate(&mut self, index: usize, instruction: &Instruction) -> Lift<()> {
        let Detail::Interpolation { attribute, channel } = instruction.detail else {
            return self.fail("the instruction is not decoded");
        };
        let (register, _) = self.vector_register(instruction.arg(Role::Result))?;
        let term = |lifter: &mut Self, term| {
            lifter.emit(Op::Input(Input::Attribute {
                attribute,
                channel,
                term,
            }))
        };
        let Some(source) = instruction.arg(Role::Source) else {
            return self.fail("the instruction is not decoded");
        };
        // The hardware evaluates P0 + i * P10 + j * P20 in two halves. Each
        // is lifted as written, because programs share first halves and
        // compute weights of their own; where both halves use a pair of
        // weights exactly as the stage handed them, the pair reads as one
        // interpolated input (`Lifter::interpolation_pair`).
        let value = match name_of(instruction) {
            "v_interp_p1_f32" => {
                if let Some((second, weights)) = self.interpolation_pair(index) {
                    // The second half writes the interpolated input; this
                    // half writes nothing.
                    self.paired.insert(second, weights);
                    return Ok(());
                }
                let weight = self.word(source)?;
                let slope = term(self, VertexTerm::P10);
                let base = term(self, VertexTerm::P0);
                self.emit(Op::Ternary(TernaryOp::FloatFused, slope, weight, base))
            }
            "v_interp_p2_f32" => match self.paired.remove(&index) {
                Some(weights) => self.emit(Op::Input(Input::Interpolated {
                    attribute,
                    channel,
                    weights,
                })),
                None => {
                    let weight = self.word(source)?;
                    let slope = term(self, VertexTerm::P20);
                    let old = self.emit(Op::Get(Var::Vector(register)));
                    self.emit(Op::Ternary(TernaryOp::FloatFused, slope, weight, old))
                }
            },
            "v_interp_mov_f32" => {
                // The field picks the term: 0 for P10, 1 for P20, 2 for P0.
                let which = match source {
                    Operand::Immediate(0) => VertexTerm::P10,
                    Operand::Immediate(1) => VertexTerm::P20,
                    Operand::Immediate(2) => VertexTerm::P0,
                    _ => return self.fail("an interpolation term that does not exist"),
                };
                term(self, which)
            }
            _ => return self.fail("an interpolation of 16-bit values"),
        };
        self.set_vector(register, value);
        Ok(())
    }
}

/// Whether an operand spans more than one register.
fn wide(operand: Operand) -> bool {
    matches!(
        operand,
        Operand::Vector { count, .. } | Operand::Scalar { count, .. } if count > 1
    )
}
