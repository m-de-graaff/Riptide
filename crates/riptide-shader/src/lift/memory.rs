//! Lifting memory access: scalar loads, buffer access through a
//! descriptor, access by address, atomic operations, the group-shared
//! memory of a compute program, and the lane swizzle that is encoded as one
//! of its operations.
//!
//! Address shapes are those of section 4 of the research note. What was
//! seen in the real title's programs and agrees with it: a table's address
//! arrives in one register and the code zeroes the register above it before
//! loading through the pair; a root view's address arrives in two registers
//! and the code builds a buffer descriptor around it. Taken on trust: that
//! a scalar load ignores the low two bits of its address, and the bound
//! rules of [`crate::ir::eval::BufferView`].

use super::{CODE, Lift, Lifter, name_of};
use crate::container::xbox::HardwareStage;
use crate::gfx10::{Detail, Format, Instruction, Operand, Role};
use crate::ir::{Address, AtomicOp, AtomicPlace, BinaryOp, BufferPosition, Op, Value, eval};

/// The operation an atomic instruction on a buffer or by address names
/// after `_atomic_`, if the lift has a rule for it.
fn atomic_op(name: &str) -> Option<AtomicOp> {
    let (_, kind) = name.split_once("_atomic_")?;
    Some(match kind {
        "swap" => AtomicOp::Swap,
        "cmpswap" => AtomicOp::CompareSwap,
        "add" => AtomicOp::Add,
        "sub" => AtomicOp::Sub,
        "smin" => AtomicOp::MinSigned,
        "umin" => AtomicOp::MinUnsigned,
        "smax" => AtomicOp::MaxSigned,
        "umax" => AtomicOp::MaxUnsigned,
        "and" => AtomicOp::And,
        "or" => AtomicOp::Or,
        "xor" => AtomicOp::Xor,
        "inc" => AtomicOp::Increment,
        "dec" => AtomicOp::Decrement,
        _ => return None,
    })
}

/// What a memory instruction's name says about its data.
struct Access {
    /// The width of one unit in bits.
    width: u8,
    /// How many units.
    units: u8,
    /// Whether a narrow unit is sign-extended.
    signed: bool,
}

/// Reads the data shape from a load or store mnemonic.
fn access(name: &str) -> Option<Access> {
    let (width, units, signed) = if name.ends_with("_dword") {
        (32, 1, false)
    } else if name.ends_with("_dwordx2") {
        (32, 2, false)
    } else if name.ends_with("_dwordx3") {
        (32, 3, false)
    } else if name.ends_with("_dwordx4") {
        (32, 4, false)
    } else if name.ends_with("_dwordx8") {
        (32, 8, false)
    } else if name.ends_with("_dwordx16") {
        (32, 16, false)
    } else if name.ends_with("_ubyte") || name.ends_with("_byte") {
        (8, 1, false)
    } else if name.ends_with("_sbyte") {
        (8, 1, true)
    } else if name.ends_with("_ushort") || name.ends_with("_short") {
        (16, 1, false)
    } else if name.ends_with("_sshort") {
        (16, 1, true)
    } else {
        return None;
    };
    Some(Access {
        width,
        units,
        signed,
    })
}

impl Lifter<'_> {
    /// Adds a 32-bit offset to a 64-bit address, carrying into the high
    /// half.
    fn offset_address(&mut self, low: Value, high: Value, offset: Value) -> Address {
        let sum = self.binary(BinaryOp::Add, low, offset);
        let carry = self.emit(Op::Carry {
            a: low,
            b: offset,
            carry_in: Value::Flag(false),
        });
        let carry = self.number_of_flag(carry);
        Address {
            low: sum,
            high: self.binary(BinaryOp::Add, high, carry),
        }
    }

    /// Sign-extends a narrow unit a load returned.
    fn extend(&mut self, value: Value, shape: &Access) -> Value {
        if !shape.signed {
            return value;
        }
        let shift = Value::Word(32 - u32::from(shape.width));
        let up = self.binary(BinaryOp::ShiftLeft, value, shift);
        self.binary(BinaryOp::ShiftRightSigned, up, shift)
    }

    /// The four words of a buffer descriptor held in scalar registers.
    fn descriptor(&mut self, operand: Option<Operand>) -> Lift<[Value; 4]> {
        let (first, count) = self.scalar_register(operand)?;
        if count != 4 {
            return self.fail("a buffer descriptor that is not four registers");
        }
        Ok([
            self.number(first)?,
            self.number(first + 1)?,
            self.number(first + 2)?,
            self.number(first + 3)?,
        ])
    }

    /// Lifts a memory instruction.
    pub(super) fn memory(&mut self, instruction: &Instruction) -> Lift<()> {
        match instruction.format {
            Format::Smem => self.scalar_memory(instruction),
            Format::Mubuf | Format::Mtbuf => self.buffer(instruction),
            _ => self.by_address(instruction),
        }
    }

    /// SMEM: loads into scalar registers.
    fn scalar_memory(&mut self, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        let Detail::ScalarMemory { offset, .. } = instruction.detail else {
            return self.fail("the instruction is not decoded");
        };
        if matches!(name, "s_gl1_inv" | "s_dcache_inv") {
            return Ok(());
        }
        let through_descriptor = name.starts_with("s_buffer_load_dword");
        if !through_descriptor && !name.starts_with("s_load_dword") {
            return self.fail("a scalar memory operation the lift has no rule for");
        }
        let Some(shape) = access(name) else {
            return self.fail("a scalar memory operation the lift has no rule for");
        };
        let (target, count) = self.scalar_register(instruction.arg(Role::Result))?;
        if count != shape.units {
            return self.fail("the instruction is not decoded");
        }
        if offset < 0 {
            return self.fail("a scalar load with a negative offset");
        }
        let mut position = Value::Word(offset as u32);
        if let Some(register) = instruction.arg(Role::Offset) {
            let register = self.word(register)?;
            position = self.binary(BinaryOp::Add, position, register);
        }
        // Everything is read before anything is written: the target often
        // overlaps the base.
        let values = if through_descriptor {
            let descriptor = self.descriptor(instruction.arg(Role::Base))?;
            let offset = self.binary(BinaryOp::And, position, Value::Word(!3));
            self.emit_many(Op::LoadBuffer {
                descriptor,
                position: BufferPosition {
                    index: Value::Word(0),
                    offset,
                },
                width: 32,
                words: shape.units,
            })
        } else {
            let (base, base_count) = self.scalar_register(instruction.arg(Role::Base))?;
            if base_count != 2 {
                return self.fail("the instruction is not decoded");
            }
            let from_code = self.state.forms[usize::from(base)] & CODE != 0;
            let low = self.number(base)?;
            if from_code {
                // A table of constants after the code: the base is the
                // program's own address, so the load reads code words.
                let offset = self.binary(BinaryOp::Add, low, position);
                let offset = self.binary(BinaryOp::And, offset, Value::Word(!3));
                self.emit_many(Op::LoadCode {
                    offset,
                    words: shape.units,
                })
            } else {
                let high = self.number(base + 1)?;
                let mut address = self.offset_address(low, high, position);
                address.low = self.binary(BinaryOp::And, address.low, Value::Word(!3));
                self.emit_many(Op::LoadMemory {
                    address,
                    width: 32,
                    words: shape.units,
                })
            }
        };
        for (index, value) in values.into_iter().enumerate() {
            self.set_number(target + index as u8, value);
        }
        Ok(())
    }

    /// MUBUF: loads and stores of vector registers through a descriptor.
    fn buffer(&mut self, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        if name.ends_with("_inv") {
            // Cache invalidations read nothing.
            return Ok(());
        }
        let Detail::Buffer {
            offset,
            offset_enable,
            index_enable,
            glc,
            tfe,
            format,
            ..
        } = instruction.detail
        else {
            return self.fail("the instruction is not decoded");
        };
        let load = name.contains("buffer_load_");
        let store = name.contains("buffer_store_");
        let atomic = name.contains("_atomic_");
        if !load && !store && !atomic {
            return self.fail("a buffer operation the lift has no rule for");
        }
        if tfe {
            return self.fail("a buffer load that reports a miss");
        }
        // A typed access converts by a data format and names how many
        // channels it moves.
        let typed = name.contains("_format_");
        if name.contains("_d16_") {
            return self.fail("a typed buffer access with 16-bit data");
        }
        let channels = match name.rsplit('_').next() {
            Some("x") => 1u8,
            Some("xy") => 2,
            Some("xyz") => 3,
            Some("xyzw") => 4,
            _ => 0,
        };
        let shape = match access(name) {
            Some(shape) => shape,
            None if typed && channels != 0 => Access {
                width: 32,
                units: channels,
                signed: false,
            },
            None if atomic => Access {
                width: 32,
                units: 1,
                signed: false,
            },
            None => return self.fail("a buffer operation the lift has no rule for"),
        };
        let descriptor = self.descriptor(instruction.arg(Role::Resource))?;
        // The address registers: the index first if there is one, then the
        // offset if there is one.
        let mut index = Value::Word(0);
        let mut position = Value::Word(u32::from(offset));
        if let Some(Operand::Vector { first, .. }) = instruction.arg(Role::Address) {
            let mut register = first;
            if index_enable {
                index = self.vector_word(register)?;
                register = register.wrapping_add(1);
            }
            if offset_enable {
                let value = self.vector_word(register)?;
                position = self.binary(BinaryOp::Add, position, value);
            }
        }
        if let Some(register) = instruction.arg(Role::Offset) {
            let register = self.word(register)?;
            position = self.binary(BinaryOp::Add, position, register);
        }
        let position = BufferPosition {
            index,
            offset: position,
        };
        if atomic {
            let (data, _) = self.vector_register(instruction.arg(Role::Data))?;
            let place = AtomicPlace::Buffer {
                descriptor,
                position,
            };
            // With the GLC bit the old word comes back in the data
            // register.
            return self.atomic(name, place, data, glc.then_some(data));
        }
        if typed {
            // The format is in the instruction (the typed buffer format) or
            // in bits 12 to 18 of the descriptor's word 3.
            let format = match format {
                Some(code) => {
                    if eval::BufferFormat::from_code(u32::from(code)).is_none() {
                        return self.fail("a data format the lift does not know");
                    }
                    Value::Word(u32::from(code))
                }
                None => {
                    let shifted = self.binary(BinaryOp::ShiftRight, descriptor[3], Value::Word(12));
                    self.binary(BinaryOp::And, shifted, Value::Word(0x7F))
                }
            };
            if load {
                let (target, count) = self.vector_register(instruction.arg(Role::Result))?;
                if count != channels {
                    return self.fail("the instruction is not decoded");
                }
                let values = self.emit_many(Op::LoadFormat {
                    descriptor,
                    position,
                    format,
                });
                for (index, &value) in values.iter().take(usize::from(channels)).enumerate() {
                    self.set_vector(target.wrapping_add(index as u8), value);
                }
            } else {
                let (data, count) = self.vector_register(instruction.arg(Role::Data))?;
                if count != channels {
                    return self.fail("the instruction is not decoded");
                }
                let mut values = [Value::Word(0); 4];
                for (index, value) in values.iter_mut().take(usize::from(channels)).enumerate() {
                    *value = self.vector_word(data.wrapping_add(index as u8))?;
                }
                let guard = self.live();
                self.emit(Op::StoreFormat {
                    descriptor,
                    position,
                    format,
                    values,
                    guard,
                });
            }
            return Ok(());
        }
        if load {
            let (target, count) = self.vector_register(instruction.arg(Role::Result))?;
            if count != shape.units {
                return self.fail("the instruction is not decoded");
            }
            let values = self.emit_many(Op::LoadBuffer {
                descriptor,
                position,
                width: shape.width,
                words: shape.units,
            });
            for (index, value) in values.into_iter().enumerate() {
                let value = self.extend(value, &shape);
                self.set_vector(target.wrapping_add(index as u8), value);
            }
        } else {
            let (data, count) = self.vector_register(instruction.arg(Role::Data))?;
            if count != shape.units {
                return self.fail("the instruction is not decoded");
            }
            let values = (0..count)
                .map(|index| self.vector_word(data.wrapping_add(index)))
                .collect::<Lift<Vec<Value>>>()?;
            let guard = self.live();
            self.emit(Op::StoreBuffer {
                descriptor,
                position,
                width: shape.width,
                values,
                guard,
            });
        }
        Ok(())
    }

    /// FLAT and GLOBAL: loads and stores of vector registers by address.
    fn by_address(&mut self, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        let Detail::Flat {
            offset, lds, glc, ..
        } = instruction.detail
        else {
            return self.fail("the instruction is not decoded");
        };
        if lds {
            return self.fail("group-shared memory reached by address");
        }
        let load = name.contains("_load_");
        let store = name.contains("_store_");
        let atomic = name.contains("_atomic_");
        let shape = match access(name) {
            Some(shape) if load || store => shape,
            _ if atomic => Access {
                width: 32,
                units: 1,
                signed: false,
            },
            _ => return self.fail("a memory operation the lift has no rule for"),
        };
        // The address is a register pair, or a scalar pair plus a 32-bit
        // vector offset; the constant is signed.
        let (address_register, address_count) =
            self.vector_register(instruction.arg(Role::Address))?;
        let first = self.vector_word(address_register)?;
        let (low, high) = match instruction.arg(Role::Base) {
            Some(base) => {
                let base_low = self.word_at(base, 0)?;
                let base_high = self.word_at(base, 1)?;
                let address = self.offset_address(base_low, base_high, first);
                (address.low, address.high)
            }
            None => {
                if address_count != 2 {
                    return self.fail("the instruction is not decoded");
                }
                let second = self.vector_word(address_register.wrapping_add(1))?;
                (first, second)
            }
        };
        let address = if offset >= 0 {
            self.offset_address(low, high, Value::Word(offset as u32))
        } else {
            // Subtracting borrows from the high half.
            let amount = Value::Word(u32::from(offset.unsigned_abs()));
            let borrow = self.emit(Op::Borrow {
                a: low,
                b: amount,
                borrow_in: Value::Flag(false),
            });
            let borrow = self.number_of_flag(borrow);
            Address {
                low: self.binary(BinaryOp::Sub, low, amount),
                high: self.binary(BinaryOp::Sub, high, borrow),
            }
        };
        if atomic {
            let (data, _) = self.vector_register(instruction.arg(Role::Data))?;
            let result = match instruction.arg(Role::Result) {
                Some(Operand::Vector { first, .. }) if glc => Some(first),
                _ => None,
            };
            return self.atomic(name, AtomicPlace::Memory(address), data, result);
        }
        if load {
            let (target, count) = self.vector_register(instruction.arg(Role::Result))?;
            if count != shape.units {
                return self.fail("the instruction is not decoded");
            }
            let values = self.emit_many(Op::LoadMemory {
                address,
                width: shape.width,
                words: shape.units,
            });
            for (index, value) in values.into_iter().enumerate() {
                let value = self.extend(value, &shape);
                self.set_vector(target.wrapping_add(index as u8), value);
            }
        } else {
            let (data, count) = self.vector_register(instruction.arg(Role::Data))?;
            if count != shape.units {
                return self.fail("the instruction is not decoded");
            }
            let values = (0..count)
                .map(|index| self.vector_word(data.wrapping_add(index)))
                .collect::<Lift<Vec<Value>>>()?;
            let guard = self.live();
            self.emit(Op::StoreMemory {
                address,
                width: shape.width,
                values,
                guard,
            });
        }
        Ok(())
    }

    /// An atomic operation on a word of memory. `data` is the register of
    /// the operand (and, for a compare-and-swap, the comparand in the
    /// register after it: the new word comes first, as the research note
    /// has it). `result` is the register the old word goes to, if the
    /// instruction returns it.
    fn atomic(
        &mut self,
        name: &'static str,
        place: AtomicPlace,
        data: u8,
        result: Option<u8>,
    ) -> Lift<()> {
        if name.ends_with("_x2") {
            return self.fail("an atomic operation on a 64-bit word");
        }
        let Some(op) = atomic_op(name) else {
            return self.fail("an atomic operation the lift has no rule for");
        };
        let value = self.vector_word(data)?;
        let compare = if op == AtomicOp::CompareSwap {
            self.vector_word(data.wrapping_add(1))?
        } else {
            Value::Word(0)
        };
        let guard = self.live();
        let old = self.emit(Op::Atomic {
            op,
            place,
            value,
            compare,
            guard,
        });
        if let Some(result) = result {
            self.set_vector(result, old);
        }
        Ok(())
    }

    /// DS: the group-shared memory of a compute program, and the lane
    /// swizzle within a quad that is encoded as one of its operations.
    ///
    /// The address is the lane's word of the address register plus a
    /// constant: one 16-bit offset, or for the forms on two addresses one
    /// offset byte each, counted in units (in 64 units for the `st64`
    /// forms). Taken on trust from the research note and AMD's public
    /// description: which data register of a compare-and-store is the
    /// comparand (the first), and that the forms named `rtn` give the word
    /// as it was.
    pub(super) fn shared(&mut self, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        let Detail::Lds {
            offset0,
            offset1,
            gds,
        } = instruction.detail
        else {
            return self.fail("the instruction is not decoded");
        };
        if name == "ds_swizzle_b32" {
            return self.swizzle(instruction, offset0, offset1);
        }
        if self.stage != HardwareStage::Compute {
            return self.fail("group-shared memory outside a compute program");
        }
        if gds {
            return self.fail("memory shared between programs");
        }
        let Some(rest) = name.strip_prefix("ds_") else {
            return self.fail("a group-shared memory operation the lift has no rule for");
        };
        if rest.contains("d16") || rest.contains("addtid") || rest.contains("src2") {
            return self.fail("a group-shared memory operation the lift has no rule for");
        }
        let Some((kind, ty)) = rest.rsplit_once('_') else {
            return self.fail("a group-shared memory operation the lift has no rule for");
        };
        // The unit: its width, how many of them one address holds, and
        // whether a narrow one is sign-extended when read.
        let (width, units, signed) = match ty {
            "b8" | "u8" => (8u8, 1u8, false),
            "i8" => (8, 1, true),
            "b16" | "u16" => (16, 1, false),
            "i16" => (16, 1, true),
            "b32" | "u32" | "i32" => (32, 1, false),
            "b64" => (32, 2, false),
            _ => return self.fail("a group-shared memory operation the lift has no rule for"),
        };
        let unit_bytes = u32::from(width / 8) * u32::from(units);
        let (address, _) = self.vector_register(instruction.arg(Role::Address))?;
        let base = self.vector_word(address)?;
        // The byte offsets the operation works at.
        let (kind, offsets): (&str, Vec<u32>) = match kind {
            "read2" | "write2" => (
                &kind[..kind.len() - 1],
                vec![
                    u32::from(offset0) * unit_bytes,
                    u32::from(offset1) * unit_bytes,
                ],
            ),
            "read2st64" | "write2st64" => (
                &kind[..kind.len() - 5],
                vec![
                    u32::from(offset0) * unit_bytes * 64,
                    u32::from(offset1) * unit_bytes * 64,
                ],
            ),
            _ => (kind, vec![u32::from(offset0) | u32::from(offset1) << 8]),
        };
        let data: Vec<u8> = instruction
            .args
            .iter()
            .filter(|arg| arg.role == Role::Data)
            .filter_map(|arg| match arg.operand {
                Operand::Vector { first, .. } => Some(first),
                _ => None,
            })
            .collect();
        match kind {
            "read" => {
                let (target, _) = self.vector_register(instruction.arg(Role::Result))?;
                let mut register = target;
                // Everything is read before anything is written.
                let mut values = Vec::new();
                for offset in offsets {
                    let offset = self.binary(BinaryOp::Add, base, Value::Word(offset));
                    values.extend(self.emit_many(Op::LoadShared {
                        offset,
                        width,
                        words: units,
                    }));
                }
                let shape = Access {
                    width,
                    units,
                    signed,
                };
                for value in values {
                    let value = self.extend(value, &shape);
                    self.set_vector(register, value);
                    register = register.wrapping_add(1);
                }
                Ok(())
            }
            "write" => {
                if data.len() != offsets.len() {
                    return self.fail("the instruction is not decoded");
                }
                let guard = self.live();
                for (offset, &register) in offsets.into_iter().zip(&data) {
                    let offset = self.binary(BinaryOp::Add, base, Value::Word(offset));
                    let mut values = Vec::new();
                    for unit in 0..units {
                        values.push(self.vector_word(register.wrapping_add(unit))?);
                    }
                    self.emit(Op::StoreShared {
                        offset,
                        width,
                        values,
                        guard,
                    });
                }
                Ok(())
            }
            _ => {
                // The atomic operations, with or without the old word
                // returned.
                let returns = kind.ends_with("_rtn");
                let op = match kind.strip_suffix("_rtn").unwrap_or(kind) {
                    "add" => AtomicOp::Add,
                    "sub" => AtomicOp::Sub,
                    "inc" => AtomicOp::Increment,
                    "dec" => AtomicOp::Decrement,
                    "min" if ty == "i32" => AtomicOp::MinSigned,
                    "min" if ty == "u32" => AtomicOp::MinUnsigned,
                    "max" if ty == "i32" => AtomicOp::MaxSigned,
                    "max" if ty == "u32" => AtomicOp::MaxUnsigned,
                    "and" => AtomicOp::And,
                    "or" => AtomicOp::Or,
                    "xor" => AtomicOp::Xor,
                    "wrxchg" => AtomicOp::Swap,
                    "cmpst" => AtomicOp::CompareSwap,
                    _ => {
                        return self
                            .fail("a group-shared memory operation the lift has no rule for");
                    }
                };
                if (width, units) != (32, 1) {
                    return self.fail("an atomic operation on other than a 32-bit word");
                }
                let (value, compare) = match (op, data.as_slice()) {
                    // The comparand comes first, then the new word.
                    (AtomicOp::CompareSwap, &[compare, new]) => {
                        (self.vector_word(new)?, self.vector_word(compare)?)
                    }
                    (AtomicOp::CompareSwap, _) => {
                        return self.fail("the instruction is not decoded");
                    }
                    (_, &[value, ..]) => (self.vector_word(value)?, Value::Word(0)),
                    _ => return self.fail("the instruction is not decoded"),
                };
                let offset = self.binary(BinaryOp::Add, base, Value::Word(offsets[0]));
                let guard = self.live();
                let old = self.emit(Op::Atomic {
                    op,
                    place: AtomicPlace::Shared(offset),
                    value,
                    compare,
                    guard,
                });
                if returns {
                    let (target, _) = self.vector_register(instruction.arg(Role::Result))?;
                    self.set_vector(target, old);
                }
                Ok(())
            }
        }
    }

    /// The lane swizzle within a quad. With bit 15 of the offset set, the
    /// low 8 bits pick a source lane of the same quad for each of its four
    /// lanes. A source lane that is not live gives zero. No memory is
    /// touched.
    fn swizzle(&mut self, instruction: &Instruction, offset0: u8, offset1: u8) -> Lift<()> {
        if offset1 & 0x80 == 0 {
            return self.fail("a lane swizzle across quads");
        }
        let lanes = [
            offset0 & 3,
            offset0 >> 2 & 3,
            offset0 >> 4 & 3,
            offset0 >> 6 & 3,
        ];
        let (target, _) = self.vector_register(instruction.arg(Role::Result))?;
        let (source, _) = self.vector_register(instruction.arg(Role::Address))?;
        let value = self.vector_word(source)?;
        let active = self.live();
        let swizzled = self.emit(Op::QuadPermute {
            value,
            lanes,
            active,
        });
        self.set_vector(target, swizzled);
        Ok(())
    }
}
