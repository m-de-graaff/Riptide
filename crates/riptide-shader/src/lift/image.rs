//! Lifting image instructions: sampling, gathering, loads, stores and size
//! queries.
//!
//! The order of the address registers and the meaning of the name's
//! suffixes are those of section 9 of the research note, which agree with
//! LLVM's description of the instruction set. The descriptor is read as
//! design decision T6 lays it out: the texture's place in the host heap is
//! word 5 of the eight, its format bits word 6; the sampler's place is bits
//! 0 to 11 of word 3 of its four. A sampler whose four words are constants
//! of the code is one the root signature fixes, and is passed on as those
//! words.

use super::{Lift, Lifter, name_of};
use crate::gfx10::{Detail, Instruction, Operand, Role};
use crate::ir::{BinaryOp, Dimension, ImageKind, ImageOp, Level, Op, Sampler, UnaryOp, Value, Var};

impl Lifter<'_> {
    /// Lifts an image instruction.
    pub(super) fn image(&mut self, instruction: &Instruction) -> Lift<()> {
        let name = name_of(instruction);
        let Detail::Image {
            component_mask,
            dimension,
            unnormalized,
            r128,
            a16,
            d16,
            tfe,
            lwe,
            ..
        } = instruction.detail
        else {
            return self.fail("the instruction is not decoded");
        };
        if r128 {
            return self.fail("an image access through a four-word descriptor");
        }
        if d16 {
            return self.fail("an image access with 16-bit data");
        }
        if tfe || lwe {
            return self.fail("an image access that reports a miss");
        }
        let dimension = Dimension::from_code(dimension);

        // What the name says.
        let sampling = name
            .strip_prefix("image_sample")
            .map(|rest| (false, rest))
            .or_else(|| name.strip_prefix("image_gather4").map(|rest| (true, rest)));
        let mut has = Suffixes::default();
        let kind = if let Some((gather, rest)) = sampling {
            for token in rest.split('_').filter(|token| !token.is_empty()) {
                match token {
                    "o" => has.offset = true,
                    "b" => has.bias = true,
                    "c" => has.compare = true,
                    "d" => has.gradients = true,
                    "l" => has.level = true,
                    "lz" => has.level_zero = true,
                    "cl" => has.clamp = true,
                    _ => return self.fail("a sampling variant the lift has no rule for"),
                }
            }
            if gather {
                if component_mask.count_ones() != 1 {
                    return self.fail("a gather of other than one channel");
                }
                ImageKind::Gather {
                    channel: component_mask.trailing_zeros() as u8,
                }
            } else {
                ImageKind::Sample
            }
        } else {
            match name {
                "image_load" => ImageKind::Load,
                "image_load_mip" => {
                    has.mip = true;
                    ImageKind::Load
                }
                "image_store" | "image_store_mip" => {
                    has.mip = name == "image_store_mip";
                    // Filled in below, once the data registers are read.
                    ImageKind::Store {
                        values: [Value::Word(0); 4],
                        guard: Value::Flag(false),
                    }
                }
                "image_get_resinfo" => ImageKind::Info,
                "image_get_lod" => ImageKind::QueryLevel,
                _ => return self.fail("an image operation the lift has no rule for"),
            }
        };

        // The address registers, one word each, in order.
        let mut address = Vec::new();
        for arg in &instruction.args {
            if arg.role != Role::Address {
                continue;
            }
            let Operand::Vector { first, count } = arg.operand else {
                return self.fail("the instruction is not decoded");
            };
            for index in 0..count {
                address.push(self.emit(Op::Get(Var::Vector(first.wrapping_add(index)))));
            }
        }
        // With 16-bit addresses the gradients, the coordinates and what
        // follows them are packed two to a register, low half first: 16-bit
        // floats for sampling, 16-bit integers for loads and stores. The
        // offset, the bias and the reference keep a register each, and the
        // bias is then a 16-bit float in its low half. (Taken from the
        // research note and LLVM; the tests check that the two interpreters
        // agree on it, not that the hardware does.)
        let floats = sampling.is_some() || matches!(kind, ImageKind::QueryLevel);
        let mut reader = Packed {
            registers: address.into_iter(),
            pending: None,
            packed: a16,
            floats,
        };
        let offset = match has.offset {
            true => Some(self.whole(&mut reader)?),
            false => None,
        };
        let bias = match has.bias {
            true => {
                let register = self.whole(&mut reader)?;
                Some(if a16 {
                    let half = self.binary(BinaryOp::And, register, Value::Word(0xFFFF));
                    self.unary(UnaryOp::FloatFromHalf, half)
                } else {
                    register
                })
            }
            false => None,
        };
        let reference = match has.compare {
            true => Some(self.whole(&mut reader)?),
            false => None,
        };
        let mut level = if has.level_zero {
            Level::Zero
        } else {
            Level::Implicit
        };
        if has.gradients {
            let components = dimension.gradient_components();
            let mut dx = Vec::new();
            let mut dy = Vec::new();
            reader.pending = None;
            for _ in 0..components {
                dx.push(self.part(&mut reader)?);
            }
            reader.pending = None;
            for _ in 0..components {
                dy.push(self.part(&mut reader)?);
            }
            level = Level::Gradients(dx, dy);
        }
        let coordinate_count = match kind {
            ImageKind::Info => 1,
            _ => dimension.coordinates(),
        };
        let mut coordinates = Vec::new();
        reader.pending = None;
        for _ in 0..coordinate_count {
            coordinates.push(self.part(&mut reader)?);
        }
        if has.level {
            level = Level::Explicit(self.part(&mut reader)?);
        }
        let level_clamp = match has.clamp {
            true => Some(self.part(&mut reader)?),
            false => None,
        };
        let mip = match has.mip {
            true => Some(self.part(&mut reader)?),
            false => None,
        };

        // The descriptors.
        let (resource, resource_count) = self.scalar_register(instruction.arg(Role::Resource))?;
        if resource_count != 8 {
            return self.fail("the instruction is not decoded");
        }
        let texture = self.number(resource + 5)?;
        let format = self.number(resource + 6)?;
        let sampler = match instruction.arg(Role::Sampler) {
            Some(operand) => {
                let (first, count) = self.scalar_register(Some(operand))?;
                if count != 4 {
                    return self.fail("the instruction is not decoded");
                }
                let words = [
                    self.number(first)?,
                    self.number(first + 1)?,
                    self.number(first + 2)?,
                    self.number(first + 3)?,
                ];
                Some(match words {
                    [
                        Value::Word(a),
                        Value::Word(b),
                        Value::Word(c),
                        Value::Word(d),
                    ] => Sampler::Immediate([a, b, c, d]),
                    _ => Sampler::Heap(self.binary(BinaryOp::And, words[3], Value::Word(0xFFF))),
                })
            }
            None => None,
        };

        let data = match instruction
            .arg(Role::Result)
            .or(instruction.arg(Role::Data))
        {
            Some(Operand::Vector { first, count }) => (first, count),
            _ => return self.fail("the instruction is not decoded"),
        };
        let kind = match kind {
            ImageKind::Store { .. } => {
                // The enabled channels come from consecutive registers; a
                // channel that is not enabled is written as zero.
                let mut values = [Value::Word(0); 4];
                let mut register = data.0;
                for (channel, value) in values.iter_mut().enumerate() {
                    if component_mask >> channel & 1 != 0 {
                        *value = self.emit(Op::Get(Var::Vector(register)));
                        register = register.wrapping_add(1);
                    }
                }
                ImageKind::Store {
                    values,
                    guard: self.live(),
                }
            }
            other => other,
        };
        let stores = matches!(kind, ImageKind::Store { .. });
        let gathers = matches!(kind, ImageKind::Gather { .. });
        let results = self.emit_many(Op::Image(Box::new(ImageOp {
            kind,
            dimension,
            texture,
            format,
            sampler,
            compare: has.compare,
            offset,
            bias,
            reference,
            coordinates,
            level,
            level_clamp,
            mip,
            unnormalized,
        })));
        if stores {
            return Ok(());
        }
        // A gather writes its four texels. Anything else writes the enabled
        // channels to consecutive registers; with none enabled, the first.
        let mut register = data.0;
        let mask = if gathers {
            0xF
        } else if component_mask == 0 {
            1
        } else {
            component_mask
        };
        for (channel, &value) in results.iter().enumerate() {
            if mask >> channel & 1 != 0 {
                self.set_vector(register, value);
                register = register.wrapping_add(1);
            }
        }
        Ok(())
    }
}

/// The address registers of an image instruction as they are read off.
struct Packed {
    /// The registers not read yet.
    registers: std::vec::IntoIter<Value>,
    /// The high half of the last register split, if it is not used yet.
    pending: Option<Value>,
    /// Whether values are packed two to a register.
    packed: bool,
    /// Whether packed values are 16-bit floats (else 16-bit integers).
    floats: bool,
}

impl Lifter<'_> {
    /// The next whole address register.
    fn whole(&mut self, reader: &mut Packed) -> Lift<Value> {
        reader.pending = None;
        match reader.registers.next() {
            Some(value) => Ok(value),
            None => self.fail("an image access with too few address registers"),
        }
    }

    /// The next gradient, coordinate or level: a whole register, or with
    /// 16-bit addresses the next half of one.
    fn part(&mut self, reader: &mut Packed) -> Lift<Value> {
        if !reader.packed {
            return self.whole(reader);
        }
        let half = match reader.pending.take() {
            Some(high) => high,
            None => {
                let register = self.whole(reader)?;
                reader.pending = Some(self.binary(BinaryOp::ShiftRight, register, Value::Word(16)));
                self.binary(BinaryOp::And, register, Value::Word(0xFFFF))
            }
        };
        Ok(if reader.floats {
            self.unary(UnaryOp::FloatFromHalf, half)
        } else {
            half
        })
    }
}

/// What the suffixes of a sampling mnemonic ask for.
#[derive(Default)]
struct Suffixes {
    offset: bool,
    bias: bool,
    compare: bool,
    gradients: bool,
    level: bool,
    level_zero: bool,
    clamp: bool,
    mip: bool,
}
