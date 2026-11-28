//! Lifting memory access: scalar loads, buffer access through a
//! descriptor, access by address, and the lane swizzle that is encoded as a
//! group-shared memory operation.
//!
//! Address shapes are those of section 4 of the research note. What was
//! seen in the real title's programs and agrees with it: a table's address
//! arrives in one register and the code zeroes the register above it before
//! loading through the pair; a root view's address arrives in two registers
//! and the code builds a buffer descriptor around it. Taken on trust: that
//! a scalar load ignores the low two bits of its address, and the bound
//! rules of [`crate::ir::eval::BufferView`].

use super::{CODE, Lift, Lifter, name_of};
use crate::gfx10::{Detail, Format, Instruction, Operand, Role};
use crate::ir::{Address, BinaryOp, BufferPosition, Op, Value, Var, eval};

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
            tfe,
            format,
            ..
        } = instruction.detail
        else {
            return self.fail("the instruction is not decoded");
        };
        let load = name.contains("buffer_load_");
        let store = name.contains("buffer_store_");
        if !load && !store {
            return self.fail("an atomic buffer operation");
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
                index = self.emit(Op::Get(Var::Vector(register)));
                register = register.wrapping_add(1);
            }
            if offset_enable {
                let value = self.emit(Op::Get(Var::Vector(register)));
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
                    *value = self.emit(Op::Get(Var::Vector(data.wrapping_add(index as u8))));
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
                .map(|index| self.emit(Op::Get(Var::Vector(data.wrapping_add(index)))))
                .collect();
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
        let Detail::Flat { offset, lds, .. } = instruction.detail else {
            return self.fail("the instruction is not decoded");
        };
        if lds {
            return self.fail("group-shared memory is not lifted");
        }
        let load = name.contains("_load_");
        let store = name.contains("_store_");
        if !load && !store {
            return self.fail("an atomic memory operation");
        }
        let Some(shape) = access(name) else {
            return self.fail("a memory operation the lift has no rule for");
        };
        // The address is a register pair, or a scalar pair plus a 32-bit
        // vector offset; the constant is signed.
        let (address_register, address_count) =
            self.vector_register(instruction.arg(Role::Address))?;
        let first = self.emit(Op::Get(Var::Vector(address_register)));
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
                let second = self.emit(Op::Get(Var::Vector(address_register.wrapping_add(1))));
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
                .map(|index| self.emit(Op::Get(Var::Vector(data.wrapping_add(index)))))
                .collect();
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

    /// DS: only the lane swizzle within a quad is lifted. The rest is
    /// group-shared memory.
    pub(super) fn shared(&mut self, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        let Detail::Lds {
            offset0, offset1, ..
        } = instruction.detail
        else {
            return self.fail("the instruction is not decoded");
        };
        if name != "ds_swizzle_b32" {
            return self.fail("group-shared memory is not lifted");
        }
        // With bit 15 of the offset set, the low 8 bits pick a source lane
        // of the same quad for each of its four lanes. A source lane that
        // is not live gives zero. No memory is touched.
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
        let value = self.emit(Op::Get(Var::Vector(source)));
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
