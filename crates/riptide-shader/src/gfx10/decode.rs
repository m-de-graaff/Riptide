//! Decoding one instruction: its format, its length, its opcode and its
//! fields.
//!
//! Field positions are those of the public descriptions of the instruction
//! set (LLVM's AMDGPU backend). Each format's reader states them next to the
//! code that takes them apart. Bit 0 is the least significant bit of a word.

use super::opcodes::{self, find};
use super::operand::{LITERAL, NULL, Operand};
use super::shape::{self, ConstantPosition};
use super::{
    Arg, Detail, Format, Instruction, LanePermutation, Role, SubRegister, VectorModifiers, WaveSize,
};

/// The source 0 code that says a sub-register word follows.
const SUB_REGISTER: u32 = 0xF9;
/// The source 0 code that says a row permutation word follows.
const PERMUTE_ROWS: u32 = 0xFA;
/// The source 0 code that says an eight-lane permutation word follows.
const PERMUTE_EIGHT: u32 = 0xE9;
/// The same, also fetching inactive lanes.
const PERMUTE_EIGHT_FETCH: u32 = 0xEA;
/// The scalar address field value that means an address-based memory access
/// has no scalar base. LLVM writes the null register's code; 0x7F is what
/// the previous generation wrote, and is accepted too because it could never
/// name a usable base.
const NO_BASE: [u32; 2] = [0x7D, 0x7F];

/// Takes `width` bits of `word` starting at bit `low`.
fn bits(word: u32, low: u32, width: u32) -> u32 {
    (word >> low) & ((1u32 << width) - 1)
}

/// Tests one bit of `word`.
fn bit(word: u32, at: u32) -> bool {
    word >> at & 1 != 0
}

/// What the first word (and for some formats the second) says before any
/// field is read.
struct Shape {
    format: Format,
    opcode: u16,
    name: Option<&'static str>,
    /// The length in words, including literals and extra words.
    length: usize,
}

/// Decodes the instruction that starts at word `index` of `code`.
///
/// Returns `None` only when `index` is past the end of `code`. Every word
/// pattern decodes to something: an instruction that cannot be named comes
/// back with no mnemonic and no operands, and keeps its words.
///
/// `wave` gives the width of lane masks, which sets the width of some
/// operands (the result of a comparison, a carry, the select of a
/// conditional move).
pub fn decode_at(code: &[u32], index: usize, wave: WaveSize) -> Option<Instruction> {
    let rest = code.get(index..)?;
    let first = *rest.first()?;
    let shape = classify(first, rest.get(1).copied());
    let truncated = rest.len() < shape.length;
    let words = rest[..shape.length.min(rest.len())].to_vec();
    let mut instruction = Instruction {
        offset: (index as u32).wrapping_mul(4),
        format: shape.format,
        opcode: shape.opcode,
        mnemonic: if truncated { None } else { shape.name },
        words,
        truncated,
        args: Vec::new(),
        detail: Detail::None,
    };
    if let Some(name) = instruction.mnemonic {
        let mut fields = Fields {
            words: &instruction.words,
            name,
            wave,
            args: Vec::new(),
        };
        let detail = match instruction.format {
            Format::Sop2 | Format::Sopc | Format::Sop1 => fields.scalar(instruction.format),
            Format::Sopk => fields.scalar_immediate(),
            Format::Sopp => Detail::Immediate(fields.word(0) as u16),
            Format::Smem => fields.scalar_memory(),
            Format::Vop2 | Format::Vop1 | Format::Vopc => fields.vector_short(instruction.format),
            Format::Vop3a | Format::Vop3b => fields.vector_long(instruction.opcode),
            Format::Vop3p => fields.vector_packed(),
            Format::Vintrp => fields.interpolation(),
            Format::Ds => fields.lds(),
            Format::Mubuf | Format::Mtbuf => fields.buffer(instruction.format),
            Format::Mimg => fields.image(),
            Format::Flat | Format::Global | Format::Scratch => fields.flat(instruction.format),
            Format::Exp => fields.export(),
            Format::Unknown => Detail::None,
        };
        instruction.args = fields.args;
        instruction.detail = detail;
    }
    Some(instruction)
}

/// Finds the format, opcode and length of the instruction whose first word
/// is `first`. `second` is the word after it, if there is one; the
/// three-source formats keep their source fields there, and a source field
/// can ask for a literal.
fn classify(first: u32, second: Option<u32>) -> Shape {
    if first >> 31 == 0 {
        // Vector operations with one or two sources, and comparisons. Source
        // 0 is bits 0-8. Some of its codes say that one more word follows.
        let source0 = bits(first, 0, 9);
        let extra = matches!(
            source0,
            0xFF | SUB_REGISTER | PERMUTE_ROWS | PERMUTE_EIGHT | PERMUTE_EIGHT_FETCH
        );
        return match first >> 25 {
            0x3F => {
                let opcode = bits(first, 9, 8) as u16;
                Shape {
                    format: Format::Vop1,
                    opcode,
                    name: find(opcodes::VOP1, opcode),
                    length: 1 + usize::from(extra),
                }
            }
            0x3E => {
                let opcode = bits(first, 17, 8) as u16;
                Shape {
                    format: Format::Vopc,
                    opcode,
                    name: find(opcodes::VOPC, opcode),
                    length: 1 + usize::from(extra),
                }
            }
            opcode => {
                let opcode = opcode as u16;
                let name = find(opcodes::VOP2, opcode);
                // A multiply-add with constant always has its constant word.
                let constant = name.is_some_and(|name| shape::constant_position(name).is_some());
                Shape {
                    format: Format::Vop2,
                    opcode,
                    name,
                    length: 1 + usize::from(extra || constant),
                }
            }
        };
    }
    if first >> 30 == 0b10 {
        // Scalar operations. Sources are bits 0-7 and 8-15.
        let literal = |fields: &[u32]| 1 + usize::from(fields.contains(&u32::from(LITERAL)));
        let (source0, source1) = (bits(first, 0, 8), bits(first, 8, 8));
        return match first >> 23 {
            0x17D => {
                let opcode = bits(first, 8, 8) as u16;
                Shape {
                    format: Format::Sop1,
                    opcode,
                    name: find(opcodes::SOP1, opcode),
                    length: literal(&[source0]),
                }
            }
            0x17E => {
                let opcode = bits(first, 16, 7) as u16;
                Shape {
                    format: Format::Sopc,
                    opcode,
                    name: find(opcodes::SOPC, opcode),
                    length: literal(&[source0, source1]),
                }
            }
            0x17F => {
                let opcode = bits(first, 16, 7) as u16;
                Shape {
                    format: Format::Sopp,
                    opcode,
                    name: find(opcodes::SOPP, opcode),
                    length: 1,
                }
            }
            _ if first >> 28 == 0b1011 => {
                let opcode = bits(first, 23, 5) as u16;
                let name = find(opcodes::SOPK, opcode);
                // One operation of this format has a second, 32-bit immediate.
                let wide = name == Some("s_setreg_imm32_b32");
                Shape {
                    format: Format::Sopk,
                    opcode,
                    name,
                    length: 1 + usize::from(wide),
                }
            }
            _ => {
                let opcode = bits(first, 23, 7) as u16;
                Shape {
                    format: Format::Sop2,
                    opcode,
                    name: find(opcodes::SOP2, opcode),
                    length: literal(&[source0, source1]),
                }
            }
        };
    }
    // Everything else is two words or more, told apart by bits 26-31.
    let long_length = || {
        let sources = second.unwrap_or(0);
        let literal = [0, 9, 18]
            .iter()
            .any(|&low| bits(sources, low, 9) == u32::from(LITERAL));
        2 + usize::from(literal)
    };
    match first >> 26 {
        0x32 => {
            let opcode = bits(first, 16, 2) as u16;
            Shape {
                format: Format::Vintrp,
                opcode,
                name: find(opcodes::VINTRP, opcode),
                length: 1,
            }
        }
        0x33 => {
            let opcode = bits(first, 16, 7) as u16;
            Shape {
                format: Format::Vop3p,
                opcode,
                name: find(opcodes::VOP3P, opcode),
                length: long_length(),
            }
        }
        0x35 => {
            let opcode = bits(first, 16, 10) as u16;
            let name = opcodes::vop3_name(opcode);
            let second_result = name.is_some_and(shape::has_second_result);
            Shape {
                format: if second_result {
                    Format::Vop3b
                } else {
                    Format::Vop3a
                },
                opcode,
                name,
                length: long_length(),
            }
        }
        0x36 => {
            let opcode = bits(first, 18, 8) as u16;
            Shape {
                format: Format::Ds,
                opcode,
                name: find(opcodes::DS, opcode),
                length: 2,
            }
        }
        0x37 => {
            // Bits 14-15 choose the address space; the fourth value is unused.
            let opcode = bits(first, 18, 7) as u16;
            let (format, name) = match bits(first, 14, 2) {
                0 => (Format::Flat, find(opcodes::FLAT, opcode)),
                1 => (Format::Scratch, find(opcodes::SCRATCH, opcode)),
                2 => (Format::Global, find(opcodes::GLOBAL, opcode)),
                _ => (Format::Flat, None),
            };
            Shape {
                format,
                opcode,
                name,
                length: 2,
            }
        }
        0x38 => {
            // The opcode's eighth bit sits apart from the other seven.
            let opcode = (bits(first, 18, 7) | bits(first, 25, 1) << 7) as u16;
            Shape {
                format: Format::Mubuf,
                opcode,
                name: find(opcodes::MUBUF, opcode),
                length: 2,
            }
        }
        0x3A => {
            // The opcode's fourth bit is in the second word.
            let high = second.map_or(0, |word| bits(word, 21, 1));
            let opcode = (bits(first, 16, 3) | high << 3) as u16;
            Shape {
                format: Format::Mtbuf,
                opcode,
                name: find(opcodes::MTBUF, opcode),
                length: 2,
            }
        }
        0x3C => {
            // The opcode's eighth bit is bit 0. Bits 1-2 count extra words
            // that hold address registers.
            let opcode = (bits(first, 18, 7) | bits(first, 0, 1) << 7) as u16;
            Shape {
                format: Format::Mimg,
                opcode,
                name: find(opcodes::MIMG, opcode),
                length: 2 + bits(first, 1, 2) as usize,
            }
        }
        0x3D => {
            let opcode = bits(first, 18, 8) as u16;
            Shape {
                format: Format::Smem,
                opcode,
                name: find(opcodes::SMEM, opcode),
                length: 2,
            }
        }
        0x3E => Shape {
            format: Format::Exp,
            opcode: 0,
            name: Some("exp"),
            length: 2,
        },
        _ => Shape {
            format: Format::Unknown,
            opcode: 0,
            name: None,
            length: 1,
        },
    }
}

/// Reads the fields of one instruction into operands.
struct Fields<'a> {
    words: &'a [u32],
    name: &'static str,
    wave: WaveSize,
    args: Vec<Arg>,
}

impl Fields<'_> {
    /// The instruction's word `index`, or 0 if it has none.
    fn word(&self, index: usize) -> u32 {
        self.words.get(index).copied().unwrap_or(0)
    }

    /// The last word of the instruction, which is the literal when a source
    /// asks for one.
    fn literal(&self) -> u32 {
        self.words.last().copied().unwrap_or(0)
    }

    /// Adds an operand.
    fn push(&mut self, role: Role, operand: Operand) {
        self.args.push(Arg { role, operand });
    }

    /// Reads an 8-bit field that names a scalar register or a constant, in a
    /// place where no literal can follow.
    fn scalar_field(code: u32, count: u8) -> Operand {
        if code == u32::from(LITERAL) {
            Operand::Other(LITERAL)
        } else {
            Operand::source(code as u16, count, 0)
        }
    }

    /// SOP2, SOPC and SOP1: source 0 in bits 0-7, source 1 in bits 8-15 (not
    /// SOP1), result in bits 16-22 (not SOPC).
    fn scalar(&mut self, format: Format) -> Detail {
        let word = self.word(0);
        let literal = self.literal();
        let (result_width, width0, width1) = shape::scalar_widths(self.name);
        let result = Operand::scalar(bits(word, 16, 7) as u8, result_width);
        let source0 = Operand::source(bits(word, 0, 8) as u16, width0, literal);
        let source1 = Operand::source(bits(word, 8, 8) as u16, width1, literal);
        match format {
            Format::Sop2 => {
                self.push(Role::Result, result);
                self.push(Role::Source, source0);
                self.push(Role::Source, source1);
            }
            Format::Sopc => {
                self.push(Role::Source, source0);
                self.push(Role::Source, source1);
            }
            _ => match self.name {
                "s_getpc_b64" => self.push(Role::Result, result),
                "s_setpc_b64" | "s_rfe_b64" => self.push(Role::Source, source0),
                _ => {
                    self.push(Role::Result, result);
                    self.push(Role::Source, source0);
                }
            },
        }
        Detail::None
    }

    /// SOPK: a 16-bit immediate in bits 0-15 and a register in bits 16-22.
    /// The register is written by most operations and read by comparisons,
    /// register writes and waits.
    fn scalar_immediate(&mut self) -> Detail {
        let word = self.word(0);
        let name = self.name;
        let width = if name == "s_call_b64" { 2 } else { 1 };
        let register = Operand::scalar(bits(word, 16, 7) as u8, width);
        if name == "s_setreg_imm32_b32" {
            self.push(Role::Source, Operand::Literal(self.word(1)));
        } else if name == "s_version" {
            // Only the immediate matters.
        } else if name.starts_with("s_cmpk_")
            || name.starts_with("s_waitcnt_")
            || name.starts_with("s_subvector_loop_")
            || name == "s_setreg_b32"
        {
            self.push(Role::Source, register);
        } else {
            self.push(Role::Result, register);
        }
        Detail::Immediate(word as u16)
    }

    /// SMEM: base register pair in bits 0-5 (counted in pairs), data
    /// register in bits 6-12, DLC bit 14, GLC bit 16; in the second word the
    /// offset in bits 0-20 and an offset register in bits 25-31.
    fn scalar_memory(&mut self) -> Detail {
        let (word0, word1) = (self.word(0), self.word(1));
        let name = self.name;
        let data = bits(word0, 6, 7);
        let probe = name.starts_with("s_atc_probe");
        let no_operands = matches!(name, "s_gl1_inv" | "s_dcache_inv" | "s_dcache_wb");
        let detail = Detail::ScalarMemory {
            offset: ((word1 << 11) as i32) >> 11,
            glc: !probe && bit(word0, 16),
            dlc: !probe && bit(word0, 14),
        };
        if no_operands {
            return detail;
        }
        if probe {
            // The data field of a probe is a mode number, not a register.
            self.push(Role::Source, Operand::Immediate(data));
        } else if matches!(name, "s_memtime" | "s_memrealtime") {
            self.push(Role::Result, Operand::scalar(data as u8, 2));
            return detail;
        } else if name == "s_get_waveid_in_workgroup" {
            self.push(Role::Result, Operand::scalar(data as u8, 1));
            return detail;
        } else if name.starts_with("s_dcache_discard") {
            // Only an address.
        } else if name.contains("_load_") {
            let width = shape::data_words(name);
            self.push(Role::Result, Operand::scalar(data as u8, width));
        } else {
            let width = shape::data_words(name);
            self.push(Role::Data, Operand::scalar(data as u8, width));
        }
        // Buffer forms take a four-register descriptor, the others a 64-bit
        // address.
        let descriptor = name.starts_with("s_buffer_") || name == "s_atc_probe_buffer";
        let base_width = if descriptor { 4 } else { 2 };
        self.push(
            Role::Base,
            Operand::scalar((bits(word0, 0, 6) * 2) as u8, base_width),
        );
        let offset_register = bits(word1, 25, 7);
        if offset_register != u32::from(NULL) {
            self.push(Role::Offset, Operand::scalar(offset_register as u8, 1));
        }
        detail
    }

    /// The result of a vector operation whose result field holds `field`.
    /// Two operations write a scalar register.
    fn vector_result(&self, field: u32, width: u8) -> Operand {
        match self.name {
            "v_readlane_b32" | "v_readfirstlane_b32" => Operand::scalar(field as u8, 1),
            _ => Operand::vector(field as u8, width),
        }
    }

    /// VOP2, VOP1 and VOPC: source 0 in bits 0-8; then for VOP2 and VOPC a
    /// vector register in bits 9-16; the result in bits 17-24 (VOP2, VOP1).
    /// Source 0 can also announce a sub-register word or a lane permutation
    /// word, which then holds the real source 0.
    fn vector_short(&mut self, format: Format) -> Detail {
        let name = self.name;
        if shape::vector_no_operands(name) {
            return Detail::None;
        }
        let (word0, word1) = (self.word(0), self.word(1));
        let (result_width, widths) = shape::vector_widths(name);
        let mask = self.wave.mask_words();
        let compare = format == Format::Vopc;
        let mut modifiers = VectorModifiers::default();

        let code0 = bits(word0, 0, 9);
        let register1 = bits(word0, 9, 8);
        let mut source0 = Operand::source(code0 as u16, widths[0], word1);
        let mut source1 = Operand::vector(register1 as u8, widths[1]);
        let mut compare_result = Operand::vcc(self.wave);
        match code0 {
            SUB_REGISTER => {
                // Second word: source 0 in bits 0-7, result select 8-10,
                // result unused 11-12, clamp 13, output scale 14-15, then for
                // each source a select, sign-extend, negate and absolute bit
                // (16-21 and 24-29), and a bit that says the source is a
                // scalar or constant code (23 and 31). A comparison has a
                // scalar result in bits 8-14, used if bit 15 is set.
                let field0 = bits(word1, 0, 8);
                source0 = if bit(word1, 23) {
                    Self::scalar_field(field0, widths[0])
                } else {
                    Operand::vector(field0 as u8, widths[0])
                };
                if bit(word1, 31) {
                    source1 = Self::scalar_field(register1, widths[1]);
                }
                if compare {
                    if bit(word1, 15) {
                        compare_result = Operand::scalar(bits(word1, 8, 7) as u8, mask);
                    }
                } else {
                    modifiers.clamp = bit(word1, 13);
                    modifiers.output_scale = bits(word1, 14, 2) as u8;
                }
                modifiers.negate = (bits(word1, 20, 1) | bits(word1, 28, 1) << 1) as u8;
                modifiers.absolute = (bits(word1, 21, 1) | bits(word1, 29, 1) << 1) as u8;
                modifiers.sub_register = Some(SubRegister {
                    result_select: if compare { 6 } else { bits(word1, 8, 3) as u8 },
                    result_unused: if compare { 0 } else { bits(word1, 11, 2) as u8 },
                    source_select: [bits(word1, 16, 3) as u8, bits(word1, 24, 3) as u8],
                    source_sign_extend: [bit(word1, 19), bit(word1, 27)],
                });
            }
            PERMUTE_ROWS => {
                // Second word: source 0 in bits 0-7, control 8-16, fetch
                // inactive 18, bound control 19, negate and absolute of each
                // source 20-23, bank mask 24-27, row mask 28-31.
                source0 = Operand::vector(bits(word1, 0, 8) as u8, widths[0]);
                modifiers.negate = (bits(word1, 20, 1) | bits(word1, 22, 1) << 1) as u8;
                modifiers.absolute = (bits(word1, 21, 1) | bits(word1, 23, 1) << 1) as u8;
                modifiers.permutation = Some(LanePermutation::Rows {
                    control: bits(word1, 8, 9) as u16,
                    fetch_inactive: bit(word1, 18),
                    bound_control: bit(word1, 19),
                    bank_mask: bits(word1, 24, 4) as u8,
                    row_mask: bits(word1, 28, 4) as u8,
                });
            }
            PERMUTE_EIGHT | PERMUTE_EIGHT_FETCH => {
                // Second word: source 0 in bits 0-7, then eight 3-bit lane
                // numbers.
                source0 = Operand::vector(bits(word1, 0, 8) as u8, widths[0]);
                let mut lanes = [0u8; 8];
                for (lane, slot) in lanes.iter_mut().enumerate() {
                    *slot = bits(word1, 8 + 3 * lane as u32, 3) as u8;
                }
                modifiers.permutation = Some(LanePermutation::Eight {
                    lanes,
                    fetch_inactive: code0 == PERMUTE_EIGHT_FETCH,
                });
            }
            _ => {}
        }

        match format {
            Format::Vop1 => {
                let result = self.vector_result(bits(word0, 17, 8), result_width);
                self.push(Role::Result, result);
                self.push(Role::Source, source0);
            }
            Format::Vopc => {
                // The comparisons that write EXEC have no result operand.
                if !name.starts_with("v_cmpx_") {
                    self.push(Role::Result, compare_result);
                }
                self.push(Role::Source, source0);
                self.push(Role::Source, source1);
            }
            _ => {
                let result = self.vector_result(bits(word0, 17, 8), result_width);
                self.push(Role::Result, result);
                let carry = name.ends_with("_co_ci_u32");
                if carry {
                    self.push(Role::SecondResult, Operand::vcc(self.wave));
                }
                let constant = Operand::Literal(self.literal());
                match shape::constant_position(name) {
                    Some(ConstantPosition::Middle) => {
                        self.push(Role::Source, source0);
                        self.push(Role::Source, constant);
                        self.push(Role::Source, source1);
                    }
                    Some(ConstantPosition::Last) => {
                        self.push(Role::Source, source0);
                        self.push(Role::Source, source1);
                        self.push(Role::Source, constant);
                    }
                    None => {
                        self.push(Role::Source, source0);
                        self.push(Role::Source, source1);
                    }
                }
                if shape::takes_mask(name) {
                    self.push(Role::Source, Operand::vcc(self.wave));
                }
            }
        }
        Detail::Vector(modifiers)
    }

    /// VOP3A and VOP3B. First word: result in bits 0-7; then either absolute
    /// bits 8-10 and select bits 11-14, or (VOP3B) a scalar second result in
    /// bits 8-14; clamp bit 15. Second word: three sources of 9 bits each,
    /// output scale in bits 27-28, negate bits 29-31.
    fn vector_long(&mut self, opcode: u16) -> Detail {
        let name = self.name;
        if shape::vector_no_operands(name) {
            return Detail::None;
        }
        let (word0, word1) = (self.word(0), self.word(1));
        let literal = self.literal();
        let (result_width, mut widths) = shape::vector_widths(name);
        let mask = self.wave.mask_words();
        let second_result = shape::has_second_result(name);

        let compare = opcode < opcodes::VOP3_VOP2_BASE;
        let short_two = (opcodes::VOP3_VOP2_BASE..opcodes::VOP3_OWN_BASE).contains(&opcode);
        let short_one = (opcodes::VOP3_VOP1_BASE..opcodes::VOP3_VOP1_END).contains(&opcode);
        let source_count = if short_one {
            1
        } else if compare {
            2
        } else if short_two {
            if shape::takes_mask(name) { 3 } else { 2 }
        } else if shape::vop3_two_sources(name) {
            2
        } else {
            3
        };
        if short_two && shape::takes_mask(name) {
            widths[2] = mask;
        }

        let result_field = bits(word0, 0, 8);
        if compare {
            if !name.starts_with("v_cmpx_") {
                self.push(Role::Result, Operand::scalar(result_field as u8, mask));
            }
        } else {
            let result = self.vector_result(result_field, result_width);
            self.push(Role::Result, result);
        }
        if second_result {
            self.push(
                Role::SecondResult,
                Operand::scalar(bits(word0, 8, 7) as u8, mask),
            );
        }
        for (index, &width) in widths.iter().enumerate().take(source_count) {
            let code = bits(word1, 9 * index as u32, 9);
            self.push(Role::Source, Operand::source(code as u16, width, literal));
        }

        let mut modifiers = VectorModifiers {
            clamp: bit(word0, 15),
            output_scale: bits(word1, 27, 2) as u8,
            negate: bits(word1, 29, 3) as u8,
            ..VectorModifiers::default()
        };
        if !second_result {
            modifiers.absolute = bits(word0, 8, 3) as u8;
            modifiers.select = bits(word0, 11, 4) as u8;
        }
        Detail::Vector(modifiers)
    }

    /// VOP3P. First word: result in bits 0-7, negate-high bits 8-10, select
    /// bits 11-13, the third select-high bit 14, clamp bit 15. Second word:
    /// three sources, the other two select-high bits 27-28, negate bits
    /// 29-31.
    fn vector_packed(&mut self) -> Detail {
        let name = self.name;
        let (word0, word1) = (self.word(0), self.word(1));
        let literal = self.literal();
        let three = name.starts_with("v_pk_mad_")
            || name.starts_with("v_pk_fma_")
            || name.starts_with("v_dot")
            || name.starts_with("v_fma_mix");
        self.push(Role::Result, Operand::vector(bits(word0, 0, 8) as u8, 1));
        for index in 0..if three { 3 } else { 2 } {
            let code = bits(word1, 9 * index, 9);
            self.push(Role::Source, Operand::source(code as u16, 1, literal));
        }
        Detail::Vector(VectorModifiers {
            negate_high: bits(word0, 8, 3) as u8,
            select: bits(word0, 11, 3) as u8,
            select_high: (bits(word1, 27, 2) | bits(word0, 14, 1) << 2) as u8,
            clamp: bit(word0, 15),
            negate: bits(word1, 29, 3) as u8,
            ..VectorModifiers::default()
        })
    }

    /// VINTRP: source register in bits 0-7, channel 8-9, attribute 10-15,
    /// result 18-25.
    fn interpolation(&mut self) -> Detail {
        let word = self.word(0);
        let source = bits(word, 0, 8);
        self.push(Role::Result, Operand::vector(bits(word, 18, 8) as u8, 1));
        if self.name == "v_interp_mov_f32" {
            // The field picks which vertex's value is copied: 0, 1, 2 for
            // the parameters assemblers call p10, p20, p0.
            self.push(Role::Source, Operand::Immediate(source));
        } else {
            self.push(Role::Source, Operand::vector(source as u8, 1));
        }
        Detail::Interpolation {
            attribute: bits(word, 10, 6) as u8,
            channel: bits(word, 8, 2) as u8,
        }
    }

    /// DS. First word: two offset bytes in bits 0-15, GDS bit 17. Second
    /// word: address register in bits 0-7, two data registers in 8-15 and
    /// 16-23, result in 24-31.
    fn lds(&mut self) -> Detail {
        let (word0, word1) = (self.word(0), self.word(1));
        let operands = shape::lds_shape(self.name);
        if operands.returns {
            let pair = shape::lds_pair(self.name);
            let width = operands.width * if pair { 2 } else { 1 };
            self.push(
                Role::Result,
                Operand::vector(bits(word1, 24, 8) as u8, width),
            );
        }
        if operands.address {
            self.push(Role::Address, Operand::vector(bits(word1, 0, 8) as u8, 1));
        }
        for index in 0..u32::from(operands.data) {
            let register = bits(word1, 8 + 8 * index, 8) as u8;
            self.push(Role::Data, Operand::vector(register, operands.width));
        }
        Detail::Lds {
            offset0: bits(word0, 0, 8) as u8,
            offset1: bits(word0, 8, 8) as u8,
            gds: bit(word0, 17),
        }
    }

    /// MUBUF and MTBUF. First word: offset in bits 0-11, offset-enable 12,
    /// index-enable 13, GLC 14, DLC 15, and for MTBUF the format in bits
    /// 19-25. Second word: address register in bits 0-7, data register 8-15,
    /// descriptor in 16-20 (counted in fours), SLC 22, TFE 23, offset
    /// register 24-31.
    fn buffer(&mut self, format: Format) -> Detail {
        let (word0, word1) = (self.word(0), self.word(1));
        let name = self.name;
        let (offset_enable, index_enable) = (bit(word0, 12), bit(word0, 13));
        let tfe = bit(word1, 23);
        let detail = Detail::Buffer {
            offset: bits(word0, 0, 12) as u16,
            offset_enable,
            index_enable,
            glc: bit(word0, 14),
            dlc: bit(word0, 15),
            slc: bit(word1, 22),
            tfe,
            format: (format == Format::Mtbuf).then(|| bits(word0, 19, 7) as u8),
        };
        if name.ends_with("_inv") {
            // Cache invalidations name no registers.
            return detail;
        }
        let data = bits(word1, 8, 8) as u8;
        let width = shape::data_words(name);
        if name.contains("_load_") {
            self.push(Role::Result, Operand::vector(data, width + u8::from(tfe)));
        } else {
            self.push(Role::Data, Operand::vector(data, width));
        }
        // With both an index and an offset the address is a register pair.
        let address_width = u8::from(offset_enable) + u8::from(index_enable);
        if address_width > 0 {
            self.push(
                Role::Address,
                Operand::vector(bits(word1, 0, 8) as u8, address_width),
            );
        }
        self.push(
            Role::Resource,
            Operand::scalar((bits(word1, 16, 5) * 4) as u8, 4),
        );
        self.push(Role::Offset, Self::scalar_field(bits(word1, 24, 8), 1));
        detail
    }

    /// MIMG. First word: extra address words in bits 1-2, dimension 3-5, DLC
    /// 7, component mask 8-11, unnormalised 12, GLC 13, 128-bit descriptor
    /// 15, TFE 16, LWE 17, SLC 25. Second word: address register in bits
    /// 0-7, data register 8-15, image descriptor 16-20 and sampler 21-25
    /// (both counted in fours), 16-bit addresses 30, 16-bit data 31. Each
    /// extra word holds four more address registers.
    fn image(&mut self) -> Detail {
        let (word0, word1) = (self.word(0), self.word(1));
        let name = self.name;
        let extra_words = bits(word0, 1, 2) as u8;
        let dimension = bits(word0, 3, 3) as u8;
        let component_mask = bits(word0, 8, 4) as u8;
        let (r128, tfe, lwe) = (bit(word0, 15), bit(word0, 16), bit(word0, 17));
        let (a16, d16) = (bit(word1, 30), bit(word1, 31));

        let fixed_four = name.starts_with("image_gather4") || name.starts_with("image_bvh");
        let mut data_width = if fixed_four {
            4
        } else {
            component_mask.count_ones().max(1) as u8
        };
        if d16 {
            data_width = data_width.div_ceil(2);
        }
        data_width += u8::from(tfe || lwe);
        let data = Operand::vector(bits(word1, 8, 8) as u8, data_width);
        let stores = name.contains("_store") || name.contains("_atomic_");
        self.push(if stores { Role::Data } else { Role::Result }, data);

        let address_words = shape::image_address_words(name, dimension, a16);
        let first = bits(word1, 0, 8) as u8;
        if extra_words == 0 {
            self.push(Role::Address, Operand::vector(first, address_words));
        } else {
            // One register per address component: the first in the second
            // word, the rest a byte each in the extra words.
            self.push(Role::Address, Operand::vector(first, 1));
            let available = 4 * usize::from(extra_words);
            for index in 0..usize::from(address_words.saturating_sub(1)).min(available) {
                let word = self.word(2 + index / 4);
                let register = bits(word, 8 * (index % 4) as u32, 8) as u8;
                self.push(Role::Address, Operand::vector(register, 1));
            }
        }
        self.push(
            Role::Resource,
            Operand::scalar((bits(word1, 16, 5) * 4) as u8, if r128 { 4 } else { 8 }),
        );
        if shape::image_has_sampler(name) {
            self.push(
                Role::Sampler,
                Operand::scalar((bits(word1, 21, 5) * 4) as u8, 4),
            );
        }
        Detail::Image {
            component_mask,
            dimension,
            unnormalized: bit(word0, 12),
            glc: bit(word0, 13),
            slc: bit(word0, 25),
            dlc: bit(word0, 7),
            r128,
            a16,
            d16,
            tfe,
            lwe,
            extra_address_words: extra_words,
        }
    }

    /// FLAT, GLOBAL and SCRATCH. First word: offset in bits 0-11, DLC 12,
    /// LDS 13, GLC 16, SLC 17. Second word: address register in bits 0-7,
    /// data register 8-15, scalar base 16-22, result 24-31.
    fn flat(&mut self, format: Format) -> Detail {
        let (word0, word1) = (self.word(0), self.word(1));
        let name = self.name;
        let glc = bit(word0, 16);
        let base = bits(word1, 16, 7);
        let has_base = format != Format::Flat && !NO_BASE.contains(&base);
        let width = shape::data_words(name);
        let load = name.contains("_load_");
        let atomic = name.contains("_atomic_");

        if load {
            self.push(
                Role::Result,
                Operand::vector(bits(word1, 24, 8) as u8, width),
            );
        } else if atomic && glc {
            // An atomic operation returns the old value when GLC is set. A
            // compare-and-swap returns half of what it takes.
            let returned = if name.contains("cmpswap") {
                width / 2
            } else {
                width
            };
            self.push(
                Role::Result,
                Operand::vector(bits(word1, 24, 8) as u8, returned),
            );
        }
        let address = bits(word1, 0, 8) as u8;
        match format {
            // A flat address is always a 64-bit register pair.
            Format::Flat => self.push(Role::Address, Operand::vector(address, 2)),
            // A global address is a pair, or a 32-bit offset from a base.
            Format::Global => {
                let address_width = if has_base { 1 } else { 2 };
                self.push(Role::Address, Operand::vector(address, address_width));
            }
            // A scratch access takes a vector offset or a scalar one.
            _ => {
                if !has_base {
                    self.push(Role::Address, Operand::vector(address, 1));
                }
            }
        }
        if !load {
            self.push(Role::Data, Operand::vector(bits(word1, 8, 8) as u8, width));
        }
        if has_base {
            let base_width = if format == Format::Global { 2 } else { 1 };
            self.push(Role::Base, Operand::scalar(base as u8, base_width));
        }
        Detail::Flat {
            offset: if format == Format::Flat {
                // The hardware ignores the top bit of a flat offset.
                bits(word0, 0, 11) as i16
            } else {
                ((bits(word0, 0, 12) << 4) as u16 as i16) >> 4
            },
            lds: bit(word0, 13),
            glc,
            slc: bit(word0, 17),
            dlc: bit(word0, 12),
        }
    }

    /// EXP. First word: enable bits 0-3, target 4-9, compressed 10, done 11,
    /// valid mask 12. Second word: four source registers, a byte each.
    fn export(&mut self) -> Detail {
        let (word0, word1) = (self.word(0), self.word(1));
        let enabled = bits(word0, 0, 4) as u8;
        let compressed = bit(word0, 10);
        for channel in 0..4u32 {
            if enabled >> channel & 1 != 0 {
                // Compressed exports use two registers, each for two values.
                let slot = if compressed { channel / 2 } else { channel };
                self.push(
                    Role::Source,
                    Operand::vector(bits(word1, 8 * slot, 8) as u8, 1),
                );
            }
        }
        Detail::Export {
            target: bits(word0, 4, 6) as u8,
            enabled,
            compressed,
            done: bit(word0, 11),
            valid_mask: bit(word0, 12),
        }
    }
}
