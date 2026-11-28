//! Lifting the scalar formats: SOP2, SOP1, SOPC and SOPK.
//!
//! What each operation computes, and what it leaves in the condition code,
//! is from AMD's public description of the instruction set. The operations
//! whose width is that of a lane mask can work on masks or on numbers; the
//! module documentation of [`super`] says how that is decided.

use super::{CODE, HIGH, Lift, Lifter, MASK, Rounding, TRUE, name_of};
use crate::container::stage::StageData;
use crate::gfx10::{Detail, Instruction, Operand, Role};
use crate::ir::{BinaryOp, CompareOp, FlagOp, Input, Op, UnaryOp, Value, Var};

/// A bitwise operation of two operands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bitwise {
    And,
    Or,
    Xor,
    /// a and not b.
    AndNot,
    /// a or not b.
    OrNot,
    Nand,
    Nor,
    Xnor,
    /// not a and b.
    NotAnd,
    /// not a or b.
    NotOr,
}

impl Bitwise {
    /// Reads the operation from the part of a mnemonic after `s_`.
    fn from_name(name: &str) -> Option<Bitwise> {
        Some(match name {
            "and" => Bitwise::And,
            "or" => Bitwise::Or,
            "xor" => Bitwise::Xor,
            "andn2" => Bitwise::AndNot,
            "orn2" => Bitwise::OrNot,
            "nand" => Bitwise::Nand,
            "nor" => Bitwise::Nor,
            "xnor" => Bitwise::Xnor,
            "andn1" => Bitwise::NotAnd,
            "orn1" => Bitwise::NotOr,
            _ => return None,
        })
    }
}

/// The hardware register number of the float mode register.
const MODE_REGISTER: u16 = 1;
/// The hardware register number of the low half of the scratch memory base.
const FLAT_SCRATCH_LOW: u16 = 20;
/// The hardware register number of its high half.
const FLAT_SCRATCH_HIGH: u16 = 21;

impl Lifter<'_> {
    /// The operation on two words.
    fn bitwise_words(&mut self, kind: Bitwise, a: Value, b: Value) -> Value {
        let not = |lifter: &mut Self, value| lifter.unary(UnaryOp::Not, value);
        match kind {
            Bitwise::And => self.binary(BinaryOp::And, a, b),
            Bitwise::Or => self.binary(BinaryOp::Or, a, b),
            Bitwise::Xor => self.binary(BinaryOp::Xor, a, b),
            Bitwise::AndNot => {
                let b = not(self, b);
                self.binary(BinaryOp::And, a, b)
            }
            Bitwise::OrNot => {
                let b = not(self, b);
                self.binary(BinaryOp::Or, a, b)
            }
            Bitwise::Nand => {
                let both = self.binary(BinaryOp::And, a, b);
                not(self, both)
            }
            Bitwise::Nor => {
                let either = self.binary(BinaryOp::Or, a, b);
                not(self, either)
            }
            Bitwise::Xnor => {
                let differ = self.binary(BinaryOp::Xor, a, b);
                not(self, differ)
            }
            Bitwise::NotAnd => {
                let a = not(self, a);
                self.binary(BinaryOp::And, a, b)
            }
            Bitwise::NotOr => {
                let a = not(self, a);
                self.binary(BinaryOp::Or, a, b)
            }
        }
    }

    /// The operation on two flags.
    fn bitwise_flags(&mut self, kind: Bitwise, a: Value, b: Value) -> Value {
        match kind {
            Bitwise::And => self.flag(FlagOp::And, a, b),
            Bitwise::Or => self.flag(FlagOp::Or, a, b),
            Bitwise::Xor => self.flag(FlagOp::Xor, a, b),
            Bitwise::AndNot => {
                let b = self.not(b);
                self.flag(FlagOp::And, a, b)
            }
            Bitwise::OrNot => {
                let b = self.not(b);
                self.flag(FlagOp::Or, a, b)
            }
            Bitwise::Nand => {
                let both = self.flag(FlagOp::And, a, b);
                self.not(both)
            }
            Bitwise::Nor => {
                let either = self.flag(FlagOp::Or, a, b);
                self.not(either)
            }
            Bitwise::Xnor => {
                let differ = self.flag(FlagOp::Xor, a, b);
                self.not(differ)
            }
            Bitwise::NotAnd => {
                let a = self.not(a);
                self.flag(FlagOp::And, a, b)
            }
            Bitwise::NotOr => {
                let a = self.not(a);
                self.flag(FlagOp::Or, a, b)
            }
        }
    }

    /// Whether an operand is `EXEC` or `VCC`.
    fn is_mask_register(operand: Option<Operand>) -> bool {
        matches!(operand, Some(Operand::Exec(_) | Operand::Vcc(_)))
    }

    /// Writes numbers to the registers of a scalar result operand.
    fn set_numbers(&mut self, target: Option<Operand>, values: &[Value]) -> Lift<()> {
        match target {
            Some(Operand::Scalar { first, count }) if usize::from(count) == values.len() => {
                self.check_scalar(first + count - 1)?;
                for (index, &value) in values.iter().enumerate() {
                    self.set_number(first + index as u8, value);
                }
                Ok(())
            }
            Some(Operand::M0) if values.len() == 1 => {
                self.emit(Op::Set(Var::M0, values[0]));
                Ok(())
            }
            Some(Operand::Null) => Ok(()),
            Some(Operand::Exec(_) | Operand::Vcc(_)) => {
                self.fail("a number is written to a lane mask register")
            }
            _ => self.fail("a result operand the lift has no rule for"),
        }
    }

    /// After constants were written to the registers a mask starts at:
    /// records that they are also the mask "no lane" or "every lane".
    fn also_constant_mask(&mut self, target: Option<Operand>, values: &[Value]) {
        let Some(Operand::Scalar { first, count }) = target else {
            return;
        };
        if count != self.mask_words() {
            return;
        }
        let flag = if values.iter().all(|&value| value == Value::Word(0)) {
            false
        } else if values.iter().all(|&value| value == Value::Word(u32::MAX)) {
            true
        } else {
            return;
        };
        self.emit(Op::Set(Var::Mask(first), Value::Flag(flag)));
        let index = usize::from(first);
        self.state.forms[index] |= MASK | if flag { TRUE } else { 0 };
        if count == 2 {
            self.state.forms[index + 1] |= HIGH;
        }
    }

    /// Reads every register of an operand as numbers.
    fn words(&mut self, operand: Operand, count: u8) -> Lift<Vec<Value>> {
        (0..count)
            .map(|index| self.word_at(operand, index))
            .collect()
    }

    /// Whether any word of a list is not zero, as a flag.
    fn any_bit(&mut self, values: &[Value]) -> Value {
        let mut all = Value::Word(0);
        for &value in values {
            all = self.binary(BinaryOp::Or, all, value);
        }
        self.compare(CompareOp::NotEqual, all, Value::Word(0))
    }

    /// A bitwise operation of the given width in registers.
    fn bitwise(
        &mut self,
        kind: Bitwise,
        width: u8,
        target: Option<Operand>,
        a: Operand,
        b: Operand,
    ) -> Lift<()> {
        let mask_wide = width == self.mask_words();
        if mask_wide && (Self::is_mask_register(target) || self.only_mask(a) || self.only_mask(b)) {
            let a = self.mask(a)?;
            let b = self.mask(b)?;
            let result = self.bitwise_flags(kind, a, b);
            match target {
                Some(target) => self.set_mask(target, result)?,
                None => return self.fail("the instruction is not decoded"),
            }
            self.set_condition_from_mask(result);
            return Ok(());
        }
        let a = self.words(a, width)?;
        let b = self.words(b, width)?;
        let mut results = Vec::new();
        for (&a, &b) in a.iter().zip(&b) {
            results.push(self.bitwise_words(kind, a, b));
        }
        self.set_numbers(target, &results)?;
        self.also_constant_mask(target, &results);
        let condition = self.any_bit(&results);
        self.set_condition(condition);
        Ok(())
    }

    /// A copy, or a copy on a condition, of the given width in registers.
    /// `condition` is `None` for a plain move; otherwise the copy happens
    /// where it holds and `otherwise` is kept where it does not.
    fn copy(
        &mut self,
        width: u8,
        target: Option<Operand>,
        source: Operand,
        condition: Option<(Value, Operand)>,
    ) -> Lift<()> {
        let mask_wide = width == self.mask_words();
        let otherwise_mask = condition.is_some_and(|(_, otherwise)| self.only_mask(otherwise));
        if Self::is_mask_register(target)
            || (mask_wide && (self.only_mask(source) || otherwise_mask))
        {
            if !mask_wide {
                return self.fail("a lane mask is moved with an operation of another width");
            }
            let mut flag = self.mask(source)?;
            if let Some((condition, otherwise)) = condition {
                let otherwise = self.mask(otherwise)?;
                flag = self.select(condition, flag, otherwise);
            }
            return match target {
                Some(target) => self.set_mask(target, flag),
                None => self.fail("the instruction is not decoded"),
            };
        }
        // Numbers. A register that is both (the constants 0 and -1) keeps
        // being both, and a code address stays one.
        let both = match (source, condition) {
            (Operand::Scalar { first, count }, None)
                if mask_wide
                    && count == width
                    && self.state.forms[usize::from(first)] & MASK != 0 =>
            {
                Some(self.mask_of_register(first))
            }
            _ => None,
        };
        let code = matches!(source, Operand::Scalar { first, .. }
            if condition.is_none() && self.state.forms[usize::from(first)] & CODE != 0);
        let mut values = self.words(source, width)?;
        if let Some((condition, otherwise)) = condition {
            let otherwise = self.words(otherwise, width)?;
            for (value, otherwise) in values.iter_mut().zip(otherwise) {
                *value = self.select(condition, *value, otherwise);
            }
        }
        self.set_numbers(target, &values)?;
        if let Some(Operand::Scalar { first, count }) = target {
            let index = usize::from(first);
            if let Some(flag) = both {
                self.emit(Op::Set(Var::Mask(first), flag));
                self.state.forms[index] |= MASK | if flag == Value::Flag(true) { TRUE } else { 0 };
                if count == 2 {
                    self.state.forms[index + 1] |= HIGH;
                }
            } else {
                self.also_constant_mask(target, &values);
            }
            if code {
                self.state.forms[index] |= CODE;
            }
        }
        Ok(())
    }

    /// A 64-bit shift of the pair (low, high) by the low 6 bits of `count`.
    fn shift64(&mut self, kind: BinaryOp, low: Value, high: Value, count: Value) -> (Value, Value) {
        let count = self.binary(BinaryOp::And, count, Value::Word(63));
        let big = self.compare(CompareOp::UnsignedGreaterEqual, count, Value::Word(32));
        let small = self.binary(BinaryOp::And, count, Value::Word(31));
        let none = self.compare(CompareOp::Equal, small, Value::Word(0));
        let inverse = self.binary(BinaryOp::Sub, Value::Word(32), small);
        match kind {
            BinaryOp::ShiftLeft => {
                let low_small = self.binary(BinaryOp::ShiftLeft, low, small);
                let carried = self.binary(BinaryOp::ShiftRight, low, inverse);
                let carried = self.select(none, Value::Word(0), carried);
                let high_small = self.binary(BinaryOp::ShiftLeft, high, small);
                let high_small = self.binary(BinaryOp::Or, high_small, carried);
                (
                    self.select(big, Value::Word(0), low_small),
                    self.select(big, low_small, high_small),
                )
            }
            _ => {
                let high_small = self.binary(kind, high, small);
                let carried = self.binary(BinaryOp::ShiftLeft, high, inverse);
                let carried = self.select(none, Value::Word(0), carried);
                let low_small = self.binary(BinaryOp::ShiftRight, low, small);
                let low_small = self.binary(BinaryOp::Or, low_small, carried);
                let fill = if kind == BinaryOp::ShiftRightSigned {
                    self.binary(BinaryOp::ShiftRightSigned, high, Value::Word(31))
                } else {
                    Value::Word(0)
                };
                (
                    self.select(big, high_small, low_small),
                    self.select(big, fill, high_small),
                )
            }
        }
    }

    /// Whether the signed sum or difference `result` of `a` and `b`
    /// overflowed.
    fn signed_overflow(&mut self, a: Value, b: Value, result: Value, subtract: bool) -> Value {
        // A sum overflows when both operands differ in sign from the
        // result; a difference when the operands differ in sign and the
        // result differs from the first.
        let first = self.binary(BinaryOp::Xor, a, result);
        let second = if subtract {
            self.binary(BinaryOp::Xor, a, b)
        } else {
            self.binary(BinaryOp::Xor, b, result)
        };
        let both = self.binary(BinaryOp::And, first, second);
        self.compare(CompareOp::SignedLess, both, Value::Word(0))
    }

    /// A bit-field extract: `value` shifted right by `offset`, `width` bits
    /// kept, sign-extended if `signed`. `offset` and `width` are below 32.
    pub(super) fn extract(
        &mut self,
        value: Value,
        offset: Value,
        width: Value,
        signed: bool,
    ) -> Value {
        let shifted = self.binary(BinaryOp::ShiftRight, value, offset);
        if signed {
            let empty = self.compare(CompareOp::Equal, width, Value::Word(0));
            let up = self.binary(BinaryOp::Sub, Value::Word(32), width);
            let top = self.binary(BinaryOp::ShiftLeft, shifted, up);
            let extended = self.binary(BinaryOp::ShiftRightSigned, top, up);
            self.select(empty, Value::Word(0), extended)
        } else {
            let one = self.binary(BinaryOp::ShiftLeft, Value::Word(1), width);
            let mask = self.binary(BinaryOp::Sub, one, Value::Word(1));
            self.binary(BinaryOp::And, shifted, mask)
        }
    }

    /// Lifts an instruction of a scalar format.
    pub(super) fn scalar(&mut self, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        let target = instruction.arg(Role::Result);
        let sources: Vec<Operand> = instruction.sources().collect();
        let immediate = match instruction.detail {
            Detail::Immediate(immediate) => immediate,
            _ => 0,
        };
        let signed_immediate = Value::Word(immediate as i16 as i32 as u32);
        let source = |lifter: &Self, index: usize| -> Lift<Operand> {
            match sources.get(index) {
                Some(&operand) => Ok(operand),
                None => lifter.fail("the instruction is not decoded"),
            }
        };

        // The families named op_type, op_saveexec_type and op_wrexec_type.
        if let Some((base, ty)) = name.rsplit_once('_')
            && let Some(base) = base.strip_prefix("s_")
            && matches!(ty, "b32" | "b64")
        {
            let width = if ty == "b64" { 2 } else { 1 };
            if let Some(kind) = Bitwise::from_name(base) {
                let (a, b) = (source(self, 0)?, source(self, 1)?);
                return self.bitwise(kind, width, target, a, b);
            }
            let saving = base.strip_suffix("_saveexec");
            let writing = base.strip_suffix("_wrexec");
            if let Some(kind) = saving.or(writing).and_then(Bitwise::from_name) {
                if width != self.mask_words() {
                    return self.fail("a mask operation of another width than the wave's");
                }
                // The new mask is the operation on the source and the old
                // mask. The register gets the old mask (saveexec) or the
                // new one (wrexec).
                let operand = self.mask(source(self, 0)?)?;
                let old = self.live();
                let new = self.bitwise_flags(kind, operand, old);
                let Some(target) = target else {
                    return self.fail("the instruction is not decoded");
                };
                self.set_mask(target, if saving.is_some() { old } else { new })?;
                self.emit(Op::Set(Var::Exec, new));
                self.state.live = new == Value::Flag(true);
                self.set_condition_from_mask(new);
                return Ok(());
            }
        }

        match name {
            "s_mov_b32" | "s_mov_b64" => {
                let width = if name == "s_mov_b64" { 2 } else { 1 };
                self.copy(width, target, source(self, 0)?, None)
            }
            "s_cmov_b32" | "s_cmov_b64" => {
                let width = if name == "s_cmov_b64" { 2 } else { 1 };
                let condition = self.uniform_condition()?;
                let Some(old) = target else {
                    return self.fail("the instruction is not decoded");
                };
                self.copy(width, target, source(self, 0)?, Some((condition, old)))
            }
            "s_cselect_b32" | "s_cselect_b64" => {
                let width = if name == "s_cselect_b64" { 2 } else { 1 };
                let condition = self.uniform_condition()?;
                let (a, b) = (source(self, 0)?, source(self, 1)?);
                self.copy(width, target, a, Some((condition, b)))
            }
            "s_not_b32" | "s_not_b64" => {
                let width = if name == "s_not_b64" { 2 } else { 1 };
                let operand = source(self, 0)?;
                // not a is a nor a.
                self.bitwise(Bitwise::Nor, width, target, operand, operand)
            }
            "s_wqm_b32" | "s_wqm_b64" => {
                let width = if name == "s_wqm_b64" { 2 } else { 1 };
                if width != self.mask_words() {
                    return self.fail("a mask operation of another width than the wave's");
                }
                let mask = self.mask(source(self, 0)?)?;
                let whole = self.emit(Op::QuadAny(mask));
                match target {
                    Some(target) => self.set_mask(target, whole)?,
                    None => return self.fail("the instruction is not decoded"),
                }
                self.set_condition_from_mask(whole);
                Ok(())
            }
            "s_movk_i32" => self.set_numbers(target, &[signed_immediate]),
            "s_cmovk_i32" => {
                let condition = self.uniform_condition()?;
                let Some(old) = target else {
                    return self.fail("the instruction is not decoded");
                };
                let old = self.word(old)?;
                let value = self.select(condition, signed_immediate, old);
                self.set_numbers(target, &[value])
            }
            "s_add_u32" | "s_add_i32" | "s_sub_u32" | "s_sub_i32" | "s_addk_i32" => {
                let (first, second) = if name == "s_addk_i32" {
                    let Some(old) = target else {
                        return self.fail("the instruction is not decoded");
                    };
                    (old, None)
                } else {
                    (source(self, 0)?, Some(source(self, 1)?))
                };
                // A code address plus or minus a number stays a code
                // address.
                let is_code = |lifter: &Self, operand: Operand| {
                    matches!(operand, Operand::Scalar { first, .. }
                        if lifter.state.forms[usize::from(first)] & CODE != 0)
                };
                let first_code = is_code(self, first);
                let second_code = second.is_some_and(|second| is_code(self, second));
                let code = if name.starts_with("s_sub") {
                    first_code && !second_code
                } else {
                    first_code != second_code
                };
                let a = self.word(first)?;
                let b = match second {
                    Some(second) => self.word(second)?,
                    None => signed_immediate,
                };
                let subtract = name.starts_with("s_sub");
                let result = self.binary(
                    if subtract {
                        BinaryOp::Sub
                    } else {
                        BinaryOp::Add
                    },
                    a,
                    b,
                );
                let condition = match name {
                    "s_add_u32" => self.emit(Op::Carry {
                        a,
                        b,
                        carry_in: Value::Flag(false),
                    }),
                    "s_sub_u32" => self.emit(Op::Borrow {
                        a,
                        b,
                        borrow_in: Value::Flag(false),
                    }),
                    _ => self.signed_overflow(a, b, result, subtract),
                };
                self.set_numbers(target, &[result])?;
                if let (true, Some(Operand::Scalar { first, .. })) = (code, target) {
                    self.state.forms[usize::from(first)] |= CODE;
                }
                self.set_condition(condition);
                Ok(())
            }
            "s_addc_u32" | "s_subb_u32" => {
                let carry_in = self.uniform_condition()?;
                let a = self.word(source(self, 0)?)?;
                let b = self.word(source(self, 1)?)?;
                let one = self.number_of_flag(carry_in);
                let (op, condition) = if name == "s_addc_u32" {
                    (BinaryOp::Add, Op::Carry { a, b, carry_in })
                } else {
                    (
                        BinaryOp::Sub,
                        Op::Borrow {
                            a,
                            b,
                            borrow_in: carry_in,
                        },
                    )
                };
                let partial = self.binary(op, a, b);
                let result = self.binary(op, partial, one);
                let condition = self.emit(condition);
                self.set_numbers(target, &[result])?;
                self.set_condition(condition);
                Ok(())
            }
            "s_min_i32" | "s_min_u32" | "s_max_i32" | "s_max_u32" => {
                let a = self.word(source(self, 0)?)?;
                let b = self.word(source(self, 1)?)?;
                // The condition code says the first operand was chosen.
                let chosen = self.compare(
                    match name {
                        "s_min_i32" => CompareOp::SignedLess,
                        "s_min_u32" => CompareOp::UnsignedLess,
                        "s_max_i32" => CompareOp::SignedGreater,
                        _ => CompareOp::UnsignedGreater,
                    },
                    a,
                    b,
                );
                let result = self.select(chosen, a, b);
                self.set_numbers(target, &[result])?;
                self.set_condition(chosen);
                Ok(())
            }
            "s_mul_i32" | "s_mul_hi_u32" | "s_mul_hi_i32" | "s_mulk_i32" => {
                let (a, b) = if name == "s_mulk_i32" {
                    let Some(old) = target else {
                        return self.fail("the instruction is not decoded");
                    };
                    (self.word(old)?, signed_immediate)
                } else {
                    (self.word(source(self, 0)?)?, self.word(source(self, 1)?)?)
                };
                let result = self.binary(
                    match name {
                        "s_mul_hi_u32" => BinaryOp::MulHighUnsigned,
                        "s_mul_hi_i32" => BinaryOp::MulHighSigned,
                        _ => BinaryOp::Mul,
                    },
                    a,
                    b,
                );
                self.set_numbers(target, &[result])
            }
            "s_lshl_b32" | "s_lshr_b32" | "s_ashr_i32" => {
                let a = self.word(source(self, 0)?)?;
                let b = self.word(source(self, 1)?)?;
                let result = self.binary(
                    match name {
                        "s_lshl_b32" => BinaryOp::ShiftLeft,
                        "s_lshr_b32" => BinaryOp::ShiftRight,
                        _ => BinaryOp::ShiftRightSigned,
                    },
                    a,
                    b,
                );
                self.set_numbers(target, &[result])?;
                let condition = self.any_bit(&[result]);
                self.set_condition(condition);
                Ok(())
            }
            "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" => {
                let value = source(self, 0)?;
                let low = self.word_at(value, 0)?;
                let high = self.word_at(value, 1)?;
                let count = self.word(source(self, 1)?)?;
                let (low, high) = self.shift64(
                    match name {
                        "s_lshl_b64" => BinaryOp::ShiftLeft,
                        "s_lshr_b64" => BinaryOp::ShiftRight,
                        _ => BinaryOp::ShiftRightSigned,
                    },
                    low,
                    high,
                    count,
                );
                self.set_numbers(target, &[low, high])?;
                let condition = self.any_bit(&[low, high]);
                self.set_condition(condition);
                Ok(())
            }
            "s_lshl1_add_u32" | "s_lshl2_add_u32" | "s_lshl3_add_u32" | "s_lshl4_add_u32" => {
                let shift = u32::from(name.as_bytes()[6] - b'0');
                let a = self.word(source(self, 0)?)?;
                let b = self.word(source(self, 1)?)?;
                let a = self.binary(BinaryOp::ShiftLeft, a, Value::Word(shift));
                let result = self.binary(BinaryOp::Add, a, b);
                let condition = self.emit(Op::Carry {
                    a,
                    b,
                    carry_in: Value::Flag(false),
                });
                self.set_numbers(target, &[result])?;
                self.set_condition(condition);
                Ok(())
            }
            "s_bfm_b32" => {
                let width = self.word(source(self, 0)?)?;
                let offset = self.word(source(self, 1)?)?;
                let one = self.binary(BinaryOp::ShiftLeft, Value::Word(1), width);
                let ones = self.binary(BinaryOp::Sub, one, Value::Word(1));
                let result = self.binary(BinaryOp::ShiftLeft, ones, offset);
                self.set_numbers(target, &[result])
            }
            "s_bfe_u32" | "s_bfe_i32" => {
                let value = self.word(source(self, 0)?)?;
                let field = self.word(source(self, 1)?)?;
                // The field operand holds the offset in bits 0 to 4 and the
                // width in bits 16 to 22. A width of 32 or more keeps
                // everything.
                let offset = self.binary(BinaryOp::And, field, Value::Word(31));
                let width = self.binary(BinaryOp::ShiftRight, field, Value::Word(16));
                let width = self.binary(BinaryOp::And, width, Value::Word(0x7F));
                let all = self.compare(CompareOp::UnsignedGreaterEqual, width, Value::Word(32));
                let narrow = self.extract(value, offset, width, name == "s_bfe_i32");
                let wide = self.binary(
                    if name == "s_bfe_i32" {
                        BinaryOp::ShiftRightSigned
                    } else {
                        BinaryOp::ShiftRight
                    },
                    value,
                    offset,
                );
                let result = self.select(all, wide, narrow);
                self.set_numbers(target, &[result])?;
                let condition = self.any_bit(&[result]);
                self.set_condition(condition);
                Ok(())
            }
            "s_absdiff_i32" | "s_abs_i32" => {
                let a = self.word(source(self, 0)?)?;
                let difference = if name == "s_abs_i32" {
                    a
                } else {
                    let b = self.word(source(self, 1)?)?;
                    self.binary(BinaryOp::Sub, a, b)
                };
                let negative = self.compare(CompareOp::SignedLess, difference, Value::Word(0));
                let negated = self.binary(BinaryOp::Sub, Value::Word(0), difference);
                let result = self.select(negative, negated, difference);
                self.set_numbers(target, &[result])?;
                let condition = self.any_bit(&[result]);
                self.set_condition(condition);
                Ok(())
            }
            "s_sext_i32_i8" | "s_sext_i32_i16" => {
                let shift = Value::Word(if name == "s_sext_i32_i8" { 24 } else { 16 });
                let a = self.word(source(self, 0)?)?;
                let up = self.binary(BinaryOp::ShiftLeft, a, shift);
                let result = self.binary(BinaryOp::ShiftRightSigned, up, shift);
                self.set_numbers(target, &[result])
            }
            "s_brev_b32" => {
                let a = self.word(source(self, 0)?)?;
                let result = self.unary(UnaryOp::ReverseBits, a);
                self.set_numbers(target, &[result])
            }
            "s_bitset0_b32" | "s_bitset1_b32" => {
                let Some(old) = target else {
                    return self.fail("the instruction is not decoded");
                };
                let old = self.word(old)?;
                let bit = self.word(source(self, 0)?)?;
                let bit = self.binary(BinaryOp::ShiftLeft, Value::Word(1), bit);
                let result = if name == "s_bitset1_b32" {
                    self.binary(BinaryOp::Or, old, bit)
                } else {
                    let keep = self.unary(UnaryOp::Not, bit);
                    self.binary(BinaryOp::And, old, keep)
                };
                self.set_numbers(target, &[result])
            }
            "s_bcnt1_i32_b32" | "s_bcnt1_i32_b64" | "s_bcnt0_i32_b32" | "s_bcnt0_i32_b64" => {
                let operand = source(self, 0)?;
                let width = if name.ends_with("b64") { 2 } else { 1 };
                let ones = name.starts_with("s_bcnt1");
                let result = if ones && width == self.mask_words() && self.only_mask(operand) {
                    // The number of lanes of a mask.
                    let mask = self.mask(operand)?;
                    self.emit(Op::CountLanes(mask))
                } else {
                    let mut total = Value::Word(0);
                    for word in self.words(operand, width)? {
                        let word = if ones {
                            word
                        } else {
                            self.unary(UnaryOp::Not, word)
                        };
                        let count = self.unary(UnaryOp::CountBits, word);
                        total = self.binary(BinaryOp::Add, total, count);
                    }
                    total
                };
                self.set_numbers(target, &[result])?;
                let condition = self.any_bit(&[result]);
                self.set_condition(condition);
                Ok(())
            }
            "s_ff1_i32_b32" | "s_ff1_i32_b64" => {
                let operand = source(self, 0)?;
                let width = if name.ends_with("b64") { 2 } else { 1 };
                let result = if width == self.mask_words() && self.only_mask(operand) {
                    // The first lane of a mask.
                    let mask = self.mask(operand)?;
                    self.emit(Op::FirstLaneIndex(mask))
                } else {
                    let words = self.words(operand, width)?;
                    let low = self.unary(UnaryOp::FindLowBit, words[0]);
                    if width == 1 {
                        low
                    } else {
                        let high = self.unary(UnaryOp::FindLowBit, words[1]);
                        let high_plus = self.binary(BinaryOp::Add, high, Value::Word(32));
                        let low_empty = self.compare(CompareOp::Equal, words[0], Value::Word(0));
                        let high_empty = self.compare(CompareOp::Equal, words[1], Value::Word(0));
                        let in_high = self.select(high_empty, Value::Word(u32::MAX), high_plus);
                        self.select(low_empty, in_high, low)
                    }
                };
                self.set_numbers(target, &[result])
            }
            "s_flbit_i32_b32" | "s_flbit_i32" => {
                let a = self.word(source(self, 0)?)?;
                let result = self.unary(
                    if name == "s_flbit_i32" {
                        UnaryOp::LeadingSignBits
                    } else {
                        UnaryOp::LeadingZeros
                    },
                    a,
                );
                self.set_numbers(target, &[result])
            }
            "s_bitcmp0_b32" | "s_bitcmp1_b32" => {
                let a = self.word(source(self, 0)?)?;
                let bit = self.word(source(self, 1)?)?;
                let shifted = self.binary(BinaryOp::ShiftRight, a, bit);
                let low = self.binary(BinaryOp::And, shifted, Value::Word(1));
                let expected = Value::Word(u32::from(name == "s_bitcmp1_b32"));
                let condition = self.compare(CompareOp::Equal, low, expected);
                self.set_condition(condition);
                Ok(())
            }
            "s_cmp_eq_u64" | "s_cmp_lg_u64" => {
                let (a, b) = (source(self, 0)?, source(self, 1)?);
                let equal =
                    if self.mask_words() == 2 && self.only_mask(a) && b == Operand::Integer(0) {
                        // "No lane of the mask is set."
                        let mask = self.mask(a)?;
                        let any = self.emit(Op::AnyLane(mask));
                        self.not(any)
                    } else {
                        let a = self.words(a, 2)?;
                        let b = self.words(b, 2)?;
                        let low = self.compare(CompareOp::Equal, a[0], b[0]);
                        let high = self.compare(CompareOp::Equal, a[1], b[1]);
                        self.flag(FlagOp::And, low, high)
                    };
                let condition = if name == "s_cmp_lg_u64" {
                    self.not(equal)
                } else {
                    equal
                };
                self.set_condition(condition);
                Ok(())
            }
            "s_getpc_b64" => {
                let (first, _) = self.scalar_register(target)?;
                self.set_number(first, Value::Word(instruction.end()));
                self.state.forms[usize::from(first)] |= CODE;
                self.set_number(first + 1, Value::Word(0));
                Ok(())
            }
            "s_swappc_b64" => self.fetch(),
            "s_setreg_imm32_b32" => {
                // A write to a field of a hardware register: the number in
                // bits 0 to 5, the field's first bit in 6 to 10 and its size
                // less one in 11 to 15. Programs set the float mode with it.
                let register = immediate & 0x3F;
                let offset = u32::from(immediate >> 6 & 0x1F);
                let size = u32::from(immediate >> 11 & 0x1F) + 1;
                let Some(Operand::Literal(value)) = sources.first().copied() else {
                    return self.fail("the instruction is not decoded");
                };
                match register {
                    MODE_REGISTER => {}
                    // The base address of scratch memory, low and high.
                    // Scratch memory is not lifted, so a program that sets
                    // the base and never uses it loses nothing.
                    FLAT_SCRATCH_LOW | FLAT_SCRATCH_HIGH => return Ok(()),
                    _ => {
                        return self.fail("a write to a hardware register the lift does not know");
                    }
                }
                // Bits 0 and 1 of the mode are the rounding of 32-bit
                // floats, bits 2 and 3 that of 16-bit and 64-bit floats: 0
                // to nearest even, 3 toward zero. The intermediate form
                // rounds 32-bit operations to nearest even only; for the
                // conversions to 16-bit floats it has both. The other
                // fields (denormals, exceptions) are not modelled.
                let field = |low: u32| -> Option<u32> {
                    // The value of mode bits `low` and `low + 1` if the
                    // write covers both, `None` if it covers neither.
                    let inside = |bit: u32| bit >= offset && bit < offset + size;
                    match (inside(low), inside(low + 1)) {
                        (false, false) => None,
                        (true, true) => Some(value >> (low - offset) & 3),
                        _ => Some(u32::MAX),
                    }
                };
                match field(0) {
                    None | Some(0) => {}
                    Some(_) => return self.fail("a rounding mode other than to nearest even"),
                }
                match field(2) {
                    None => {}
                    Some(0) => self.state.half_rounding = Rounding::Nearest,
                    Some(3) => self.state.half_rounding = Rounding::TowardZero,
                    Some(_) => {
                        return self
                            .fail("a rounding mode of 16-bit floats the lift has no rule for");
                    }
                }
                Ok(())
            }
            _ => {
                if let Some(kind) = name.strip_prefix("s_cmp_").or(name.strip_prefix("s_cmpk_")) {
                    let constant = name.starts_with("s_cmpk_");
                    let Some((relation, ty)) = kind.split_once('_') else {
                        return self.fail("a scalar comparison the lift has no rule for");
                    };
                    let signed = ty == "i32";
                    if !signed && ty != "u32" {
                        return self.fail("a scalar comparison the lift has no rule for");
                    }
                    let op = match (relation, signed) {
                        ("eq", _) => CompareOp::Equal,
                        ("lg", _) => CompareOp::NotEqual,
                        ("gt", true) => CompareOp::SignedGreater,
                        ("ge", true) => CompareOp::SignedGreaterEqual,
                        ("lt", true) => CompareOp::SignedLess,
                        ("le", true) => CompareOp::SignedLessEqual,
                        ("gt", false) => CompareOp::UnsignedGreater,
                        ("ge", false) => CompareOp::UnsignedGreaterEqual,
                        ("lt", false) => CompareOp::UnsignedLess,
                        ("le", false) => CompareOp::UnsignedLessEqual,
                        _ => return self.fail("a scalar comparison the lift has no rule for"),
                    };
                    let a = self.word(source(self, 0)?)?;
                    let b = if constant {
                        // The immediate is sign-extended for the signed
                        // comparisons and zero-extended for the unsigned.
                        if signed {
                            signed_immediate
                        } else {
                            Value::Word(u32::from(immediate))
                        }
                    } else {
                        self.word(source(self, 1)?)?
                    };
                    let condition = self.compare(op, a, b);
                    self.set_condition(condition);
                    return Ok(());
                }
                self.fail("a scalar operation the lift has no rule for")
            }
        }
    }

    /// Lifts the call of a vertex program's fetch routine: the attributes
    /// arrive in the vector registers the stage data names. Which other
    /// registers the routine changes is not known; none is assumed.
    fn fetch(&mut self) -> Lift<()> {
        let StageData::Vertex(vertex) = self.data else {
            return self.fail("a call in a program that is not a vertex program");
        };
        for input in vertex.inputs.clone() {
            for channel in 0..4u8 {
                if input.mask >> channel & 1 == 0 {
                    continue;
                }
                let value = self.emit(Op::Input(Input::VertexAttribute {
                    row: input.row,
                    channel,
                }));
                let Some(register) = input.register.checked_add(channel) else {
                    return self.fail("a vertex attribute in a register that does not exist");
                };
                self.emit(Op::Set(Var::Vector(register), value));
            }
        }
        Ok(())
    }
}
