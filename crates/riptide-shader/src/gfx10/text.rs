//! Printing instructions as assembly text.
//!
//! The syntax is that of LLVM's assembler for this generation: the mnemonic,
//! the operands separated by commas with the result first, then modifiers
//! separated by spaces. An operation in the three-source encoding that also
//! has a shorter one gets the suffix `_e64`; one with a sub-register word
//! gets `_sdwa`; one with a lane permutation gets `_dpp`.
//!
//! An instruction that could not be named prints as a `.long` directive with
//! its words, followed by a comment that says why.

use std::fmt::{self, Display, Formatter, Write as _};

use super::shape;
use super::{Detail, Format, Instruction, LanePermutation, Operand, Role, VectorModifiers};

/// The names of the image dimensions, by number.
const DIMENSIONS: [&str; 8] = [
    "1D",
    "2D",
    "3D",
    "CUBE",
    "1D_ARRAY",
    "2D_ARRAY",
    "2D_MSAA",
    "2D_MSAA_ARRAY",
];

/// The names of the sub-register selects, by number.
const SELECTS: [&str; 8] = [
    "BYTE_0", "BYTE_1", "BYTE_2", "BYTE_3", "WORD_0", "WORD_1", "DWORD", "7",
];

/// The names of what happens to the unselected part of a result, by number.
const UNUSED: [&str; 4] = ["UNUSED_PAD", "UNUSED_SEXT", "UNUSED_PRESERVE", "3"];

/// The messages a program can send, by number.
const MESSAGES: [(u16, &str); 12] = [
    (1, "MSG_INTERRUPT"),
    (2, "MSG_GS"),
    (3, "MSG_GS_DONE"),
    (4, "MSG_SAVEWAVE"),
    (5, "MSG_STALL_WAVE_GEN"),
    (6, "MSG_HALT_WAVES"),
    (7, "MSG_ORDERED_PS_DONE"),
    (8, "MSG_EARLY_PRIM_DEALLOC"),
    (9, "MSG_GS_ALLOC_REQ"),
    (10, "MSG_GET_DOORBELL"),
    (11, "MSG_GET_DDID"),
    (15, "MSG_SYSMSG"),
];

/// Program-control operations that take no operand.
const BARE_CONTROL: [&str; 8] = [
    "s_endpgm",
    "s_endpgm_saved",
    "s_endpgm_ordered_ps_done",
    "s_code_end",
    "s_wakeup",
    "s_barrier",
    "s_icache_inv",
    "s_wait_idle",
];

impl Display for Instruction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let Some(name) = self.mnemonic else {
            return self.unknown(f);
        };
        let mut line = String::from(name);
        match self.format {
            Format::Sopk => self.scalar_immediate(name, &mut line),
            Format::Sopp => self.control(name, &mut line),
            Format::Smem => self.scalar_memory(&mut line),
            Format::Vop2
            | Format::Vop1
            | Format::Vopc
            | Format::Vop3a
            | Format::Vop3b
            | Format::Vop3p => self.vector(&mut line),
            Format::Vintrp => self.interpolation(name, &mut line),
            Format::Ds => self.lds(name, &mut line),
            Format::Mubuf | Format::Mtbuf => self.buffer(&mut line),
            Format::Mimg => self.image(&mut line),
            Format::Flat | Format::Global | Format::Scratch => self.flat(&mut line),
            Format::Exp => self.export(&mut line),
            Format::Sop2 | Format::Sop1 | Format::Sopc | Format::Unknown => {
                let operands: Vec<String> = self
                    .args
                    .iter()
                    .map(|arg| arg.operand.to_string())
                    .collect();
                join(&mut line, &operands);
            }
        }
        f.write_str(&line)
    }
}

/// Appends operands to a line: a space, then the operands with commas.
fn join(line: &mut String, operands: &[String]) {
    if !operands.is_empty() {
        line.push(' ');
        line.push_str(&operands.join(", "));
    }
}

/// Appends a list such as ` name:[1,0,1]` from the low `count` bits of
/// `mask`.
fn bit_list(line: &mut String, name: &str, mask: u8, count: usize) {
    let items: Vec<&str> = (0..count)
        .map(|index| if mask >> index & 1 != 0 { "1" } else { "0" })
        .collect();
    let _ = write!(line, " {name}:[{}]", items.join(","));
}

/// Appends each flag whose bit is set.
fn flags(line: &mut String, list: &[(bool, &str)]) {
    for &(set, name) in list {
        if set {
            line.push(' ');
            line.push_str(name);
        }
    }
}

impl Instruction {
    /// The text of the first operand with a role, if there is one.
    fn role_text(&self, role: Role) -> Option<String> {
        self.arg(role).map(|operand| operand.to_string())
    }

    /// Prints an instruction that has no mnemonic.
    fn unknown(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = self.words.iter().map(|w| format!("{w:#010x}")).collect();
        write!(f, ".long {}", words.join(", "))?;
        if self.truncated {
            write!(
                f,
                " ; {} instruction cut off by the end",
                self.format.name()
            )
        } else if self.format == Format::Unknown {
            write!(f, " ; unknown format")
        } else {
            write!(
                f,
                " ; unknown {} opcode {:#x}",
                self.format.name(),
                self.opcode
            )
        }
    }

    /// The 16-bit immediate, or 0 for an instruction that has none.
    fn immediate(&self) -> u16 {
        match self.detail {
            Detail::Immediate(value) => value,
            _ => 0,
        }
    }

    /// Prints the operands of a scalar operation with an immediate: the
    /// register, then the immediate, except for the operations that write a
    /// hardware register, which name the immediate first.
    fn scalar_immediate(&self, name: &str, line: &mut String) {
        let immediate = self.immediate();
        let register = self.args.first().map(|arg| arg.operand.to_string());
        let operands = match name {
            "s_call_b64" => vec![register.unwrap_or_default(), (immediate as i16).to_string()],
            "s_setreg_b32" => vec![format!("{immediate:#x}"), register.unwrap_or_default()],
            // The register here is the 32-bit immediate.
            "s_setreg_imm32_b32" => vec![format!("{immediate:#x}"), register.unwrap_or_default()],
            _ => register
                .into_iter()
                .chain([format!("{immediate:#x}")])
                .collect(),
        };
        join(line, &operands);
    }

    /// Prints the operand of a program-control operation: a branch distance,
    /// wait counters, a message, or the plain immediate.
    fn control(&self, name: &str, line: &mut String) {
        let immediate = self.immediate();
        if BARE_CONTROL.contains(&name) {
            return;
        }
        if name == "s_branch" || name.starts_with("s_cbranch_") {
            // The distance in words from the next instruction, signed.
            let _ = write!(line, " {}", immediate as i16);
        } else if name == "s_waitcnt" {
            // Three counters; a counter at its greatest value is not waited
            // for and is left out, unless that would leave nothing.
            let counters = [
                ("vmcnt", immediate & 0xF | (immediate >> 14 & 3) << 4, 63),
                ("expcnt", immediate >> 4 & 7, 7),
                ("lgkmcnt", immediate >> 8 & 0xF, 15),
            ];
            let all = counters.iter().all(|&(_, value, most)| value == most);
            for (counter, value, most) in counters {
                if all || value != most {
                    let _ = write!(line, " {counter}({value})");
                }
            }
        } else if name == "s_sendmsg" || name == "s_sendmsghalt" {
            let _ = write!(line, " {}", message(immediate));
        } else {
            let _ = write!(line, " {immediate:#x}");
        }
    }

    /// Prints a scalar memory access: data, base, then the offset as a
    /// constant, a register, or both.
    fn scalar_memory(&self, line: &mut String) {
        let Detail::ScalarMemory { offset, glc, dlc } = self.detail else {
            return;
        };
        let mut operands: Vec<String> = self
            .args
            .iter()
            .filter(|arg| arg.role != Role::Offset)
            .map(|arg| arg.operand.to_string())
            .collect();
        let has_base = self.arg(Role::Base).is_some();
        // The offset prints as the 21 bits the instruction holds.
        let offset_text = format!("{:#x}", offset & 0x1F_FFFF);
        let mut tail = String::new();
        match self.role_text(Role::Offset) {
            Some(register) => {
                operands.push(register);
                if offset != 0 {
                    let _ = write!(tail, " offset:{offset_text}");
                }
            }
            None if has_base => operands.push(offset_text),
            None => {}
        }
        join(line, &operands);
        line.push_str(&tail);
        flags(line, &[(glc, "glc"), (dlc, "dlc")]);
    }

    /// Prints a vector operation of any format: the encoding suffix, the
    /// operands with their modifiers, then the modifiers that stand alone.
    fn vector(&self, line: &mut String) {
        let Detail::Vector(modifiers) = self.detail else {
            // An operation without operands.
            return;
        };
        if modifiers.sub_register.is_some() {
            line.push_str("_sdwa");
        } else if modifiers.permutation.is_some() {
            line.push_str("_dpp");
        } else if self.is_long_form() {
            line.push_str("_e64");
        }
        let packed = self.format == Format::Vop3p;
        let mut operands = Vec::new();
        let mut source = 0usize;
        for arg in &self.args {
            let mut text = arg.operand.to_string();
            if arg.role == Role::Source {
                if !packed {
                    text = decorate(text, &modifiers, source);
                }
                source += 1;
            }
            operands.push(text);
        }
        join(line, &operands);
        let sources = source;

        if packed {
            if modifiers.select != 0 {
                bit_list(line, "op_sel", modifiers.select, sources);
            }
            // By default the high half of the result comes from the high
            // halves of the sources, so only another choice is printed.
            let all = (1u8 << sources) - 1;
            if modifiers.select_high & all != all {
                bit_list(line, "op_sel_hi", modifiers.select_high, sources);
            }
            if modifiers.negate != 0 {
                bit_list(line, "neg_lo", modifiers.negate, sources);
            }
            if modifiers.negate_high != 0 {
                bit_list(line, "neg_hi", modifiers.negate_high, sources);
            }
        } else if modifiers.select != 0 {
            // One entry per source, then one for the result (bit 3).
            let result = modifiers.select >> 3 & 1;
            let mask = modifiers.select & ((1 << sources) - 1) | result << sources;
            bit_list(line, "op_sel", mask, sources + 1);
        }
        flags(line, &[(modifiers.clamp, "clamp")]);
        match modifiers.output_scale {
            1 => line.push_str(" mul:2"),
            2 => line.push_str(" mul:4"),
            3 => line.push_str(" div:2"),
            _ => {}
        }
        if let Some(sub) = modifiers.sub_register {
            // Comparisons write a whole lane mask and have no result select.
            if !matches!(self.format, Format::Vopc) {
                let _ = write!(
                    line,
                    " dst_sel:{} dst_unused:{}",
                    SELECTS[usize::from(sub.result_select & 7)],
                    UNUSED[usize::from(sub.result_unused & 3)]
                );
            }
            for index in 0..sources.min(2) {
                let select = SELECTS[usize::from(sub.source_select[index] & 7)];
                let _ = write!(line, " src{index}_sel:{select}");
            }
        }
        match modifiers.permutation {
            Some(LanePermutation::Rows {
                control,
                fetch_inactive,
                bound_control,
                bank_mask,
                row_mask,
            }) => {
                let _ = write!(
                    line,
                    " {} row_mask:{row_mask:#x} bank_mask:{bank_mask:#x}",
                    row_control(control)
                );
                flags(
                    line,
                    &[(bound_control, "bound_ctrl:1"), (fetch_inactive, "fi:1")],
                );
            }
            Some(LanePermutation::Eight {
                lanes,
                fetch_inactive,
            }) => {
                let items: Vec<String> = lanes.iter().map(|lane| lane.to_string()).collect();
                let _ = write!(line, " dpp8:[{}]", items.join(","));
                flags(line, &[(fetch_inactive, "fi:1")]);
            }
            None => {}
        }
    }

    /// Prints an interpolation: result, source, then attribute and component.
    fn interpolation(&self, name: &str, line: &mut String) {
        let Detail::Interpolation { attribute, channel } = self.detail else {
            return;
        };
        let result = self.role_text(Role::Result).unwrap_or_default();
        let source = match self.arg(Role::Source) {
            Some(Operand::Immediate(parameter)) if name == "v_interp_mov_f32" => match parameter {
                0 => "p10".to_string(),
                1 => "p20".to_string(),
                2 => "p0".to_string(),
                other => format!("invalid_param_{other}"),
            },
            Some(operand) => operand.to_string(),
            None => String::new(),
        };
        let component = ["x", "y", "z", "w"][usize::from(channel & 3)];
        let _ = write!(line, " {result}, {source}, attr{attribute}.{component}");
    }

    /// Prints a group-shared memory access: the operands, then the offset
    /// or the two offsets when they are not zero.
    fn lds(&self, name: &str, line: &mut String) {
        let Detail::Lds {
            offset0,
            offset1,
            gds,
        } = self.detail
        else {
            return;
        };
        let operands: Vec<String> = self
            .args
            .iter()
            .map(|arg| arg.operand.to_string())
            .collect();
        join(line, &operands);
        if shape::lds_pair(name) {
            if offset0 != 0 {
                let _ = write!(line, " offset0:{offset0}");
            }
            if offset1 != 0 {
                let _ = write!(line, " offset1:{offset1}");
            }
        } else {
            let offset = u16::from(offset1) << 8 | u16::from(offset0);
            if offset != 0 {
                let _ = write!(line, " offset:{offset}");
            }
        }
        flags(line, &[(gds, "gds")]);
    }

    /// Prints a buffer access: data, address, descriptor, the format of a
    /// typed access, the offset register, then flags.
    fn buffer(&self, line: &mut String) {
        let Detail::Buffer {
            offset,
            offset_enable,
            index_enable,
            glc,
            dlc,
            slc,
            tfe,
            format,
        } = self.detail
        else {
            return;
        };
        if self.args.is_empty() {
            return;
        }
        let mut operands = Vec::new();
        operands.extend(self.role_text(Role::Result));
        operands.extend(self.role_text(Role::Data));
        // An access with neither index nor offset register writes `off`.
        operands.push(
            self.role_text(Role::Address)
                .unwrap_or_else(|| "off".to_string()),
        );
        operands.extend(self.role_text(Role::Resource));
        if let Some(format) = format {
            operands.push(format!("format:{format}"));
        }
        operands.extend(self.role_text(Role::Offset));
        join(line, &operands);
        flags(line, &[(index_enable, "idxen"), (offset_enable, "offen")]);
        if offset != 0 {
            let _ = write!(line, " offset:{offset}");
        }
        flags(
            line,
            &[(glc, "glc"), (slc, "slc"), (dlc, "dlc"), (tfe, "tfe")],
        );
    }

    /// Prints an image access: data, address, descriptors, then the
    /// component mask, the dimension and flags.
    fn image(&self, line: &mut String) {
        let Detail::Image {
            component_mask,
            dimension,
            unnormalized,
            glc,
            slc,
            dlc,
            r128,
            a16,
            d16,
            tfe,
            lwe,
            extra_address_words,
        } = self.detail
        else {
            return;
        };
        let mut operands = Vec::new();
        operands.extend(self.role_text(Role::Result));
        operands.extend(self.role_text(Role::Data));
        let addresses: Vec<String> = self
            .args
            .iter()
            .filter(|arg| arg.role == Role::Address)
            .map(|arg| arg.operand.to_string())
            .collect();
        if extra_address_words == 0 {
            operands.extend(addresses);
        } else {
            // Address registers that are not consecutive are listed.
            operands.push(format!("[{}]", addresses.join(", ")));
        }
        operands.extend(self.role_text(Role::Resource));
        operands.extend(self.role_text(Role::Sampler));
        join(line, &operands);
        let _ = write!(
            line,
            " dmask:{component_mask:#x} dim:SQ_RSRC_IMG_{}",
            DIMENSIONS[usize::from(dimension & 7)]
        );
        flags(
            line,
            &[
                (unnormalized, "unorm"),
                (dlc, "dlc"),
                (glc, "glc"),
                (slc, "slc"),
                (r128, "r128"),
                (a16, "a16"),
                (tfe, "tfe"),
                (lwe, "lwe"),
                (d16, "d16"),
            ],
        );
    }

    /// Prints a memory access by address in the operand order of its
    /// address space.
    fn flat(&self, line: &mut String) {
        let Detail::Flat {
            offset,
            lds,
            glc,
            slc,
            dlc,
        } = self.detail
        else {
            return;
        };
        let mut operands = Vec::new();
        operands.extend(self.role_text(Role::Result));
        match self.format {
            Format::Flat => operands.extend(self.role_text(Role::Address)),
            // Global and scratch accesses write `off` for the part of the
            // address they do not use.
            Format::Global => {
                operands.extend(self.role_text(Role::Address));
                operands.extend(self.role_text(Role::Data));
                operands.push(
                    self.role_text(Role::Base)
                        .unwrap_or_else(|| "off".to_string()),
                );
            }
            _ => {
                operands.push(
                    self.role_text(Role::Address)
                        .unwrap_or_else(|| "off".to_string()),
                );
                operands.extend(self.role_text(Role::Data));
                operands.push(
                    self.role_text(Role::Base)
                        .unwrap_or_else(|| "off".to_string()),
                );
            }
        }
        if self.format == Format::Flat {
            operands.extend(self.role_text(Role::Data));
        }
        join(line, &operands);
        if offset != 0 {
            let _ = write!(line, " offset:{offset}");
        }
        flags(
            line,
            &[(lds, "lds"), (glc, "glc"), (slc, "slc"), (dlc, "dlc")],
        );
    }

    /// Prints an export: the target, the four values with `off` for the
    /// ones not exported, then flags.
    fn export(&self, line: &mut String) {
        let Detail::Export {
            target,
            enabled,
            compressed,
            done,
            valid_mask,
        } = self.detail
        else {
            return;
        };
        let target = match target {
            0..=7 => format!("mrt{target}"),
            8 => "mrtz".to_string(),
            9 => "null".to_string(),
            12..=16 => format!("pos{}", target - 12),
            20 => "prim".to_string(),
            32..=63 => format!("param{}", target - 32),
            other => format!("invalid_target_{other}"),
        };
        let mut sources = self.sources();
        let mut operands = Vec::new();
        for channel in 0..4 {
            if enabled >> channel & 1 != 0 {
                operands.push(sources.next().map(|s| s.to_string()).unwrap_or_default());
            } else {
                operands.push("off".to_string());
            }
        }
        let _ = write!(line, " {target} {}", operands.join(", "));
        flags(
            line,
            &[(done, "done"), (compressed, "compr"), (valid_mask, "vm")],
        );
    }
}

/// Wraps a source's text in its modifiers: sign extension, absolute value,
/// negation.
fn decorate(mut text: String, modifiers: &VectorModifiers, source: usize) -> String {
    if source >= 3 {
        return text;
    }
    let sign_extend = modifiers
        .sub_register
        .is_some_and(|sub| source < 2 && sub.source_sign_extend[source]);
    if sign_extend {
        text = format!("sext({text})");
    }
    if modifiers.absolute >> source & 1 != 0 {
        text = format!("|{text}|");
    }
    if modifiers.negate >> source & 1 != 0 {
        text = format!("-{text}");
    }
    text
}

/// The text of a row permutation's control number.
fn row_control(control: u16) -> String {
    match control {
        0x000..=0x0FF => format!(
            "quad_perm:[{},{},{},{}]",
            control & 3,
            control >> 2 & 3,
            control >> 4 & 3,
            control >> 6 & 3
        ),
        0x101..=0x10F => format!("row_shl:{}", control & 0xF),
        0x111..=0x11F => format!("row_shr:{}", control & 0xF),
        0x121..=0x12F => format!("row_ror:{}", control & 0xF),
        0x140 => "row_mirror".to_string(),
        0x141 => "row_half_mirror".to_string(),
        other => format!("dpp_ctrl:{other:#x}"),
    }
}

/// The text of a message operand: the message's name, and for the two
/// geometry shader messages the operation and stream.
fn message(immediate: u16) -> String {
    let id = immediate & 0xF;
    let name = MESSAGES
        .iter()
        .find(|&&(number, _)| number == id)
        .map(|&(_, name)| name);
    let operation = immediate >> 4 & 3;
    let stream = immediate >> 8 & 3;
    let rest = immediate & !0x33F;
    match name {
        Some(name) if (id == 2 || id == 3) && rest == 0 => {
            let operation =
                ["GS_OP_NOP", "GS_OP_CUT", "GS_OP_EMIT", "GS_OP_EMIT_CUT"][usize::from(operation)];
            format!("sendmsg({name}, {operation}, {stream})")
        }
        Some(name) if immediate == id => format!("sendmsg({name})"),
        _ => format!("{immediate:#x}"),
    }
}

/// Prints instructions one per line, each with its byte offset and its words
/// in hexadecimal before the text. A branch with a constant distance gets a
/// comment with the offset it goes to. Where the instructions are not
/// consecutive (as in the result of a walk, which skips data), a comment
/// line says how many words were skipped.
pub fn listing(instructions: &[Instruction]) -> String {
    let mut out = String::new();
    let mut expected: Option<u32> = None;
    for instruction in instructions {
        if let Some(expected) = expected
            && instruction.offset > expected
        {
            let skipped = (instruction.offset - expected) / 4;
            let _ = writeln!(out, "          ; {skipped} words not decoded");
        }
        expected = Some(instruction.end());
        let words: Vec<String> = instruction
            .words
            .iter()
            .map(|word| format!("{word:08x}"))
            .collect();
        let _ = write!(
            out,
            "{:#08x}  {:<26} {instruction}",
            instruction.offset,
            words.join(" ")
        );
        if let Some(target) = instruction.branch_target() {
            let _ = write!(out, " ; to {target:#08x}");
        }
        out.push('\n');
    }
    out
}
