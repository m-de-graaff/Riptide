//! Decoding whole programs.
//!
//! [`decode_linear`] reads instruction after instruction from the first word
//! to the last. It is right for a stretch of code known to hold only
//! instructions.
//!
//! [`walk`] is the one to use on a program as the console stores it. The
//! compiler puts tables of constants after the last instruction of a
//! program, inside the same block of code, and nothing marks where they
//! start. So the walk follows control flow instead: it starts at the first
//! word, continues past every instruction that can fall through, and visits
//! the target of every branch. Words it never reaches are left alone and
//! counted.
//!
//! A jump through a register cannot be followed in general. Compilers emit
//! one fixed sequence for jumps too far for a branch instruction, and that
//! one is recognised:
//!
//! ```text
//! s_getpc_b64  s[n:n+1]            ; address of the next instruction
//! s_add_u32    sn, sn, <literal>   ; or s_sub_u32
//! s_addc_u32   sn+1, sn+1, ...     ; carry into the high half
//! s_setpc_b64  s[n:n+1]
//! ```
//!
//! Any other register jump ends its path. That is correct for the one other
//! use seen, where a merged stage hands over to the next stage's program.

use std::collections::BTreeMap;

use super::{Format, Instruction, Operand, Role, WaveSize, decode_at};

/// How many instructions back the far-jump sequence is looked for.
const FAR_JUMP_REACH: usize = 4;

/// The result of walking a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// Every instruction reached, ordered by offset.
    pub instructions: Vec<Instruction>,
    /// How many words of the code belong to no reached instruction. These
    /// are data, padding, or code only reachable through a register jump.
    pub unreached_words: usize,
    /// How many branches went somewhere that cannot be code: outside the
    /// program, or into the middle of an instruction already decoded. A
    /// correct decode of a correct program has none.
    pub stray_branches: usize,
    /// Whether some path ran past the last word without an instruction that
    /// ends the program. A correct decode of a correct program does not.
    pub runs_off_end: bool,
}

/// Decodes every word of `code` in order, one instruction after another.
pub fn decode_linear(code: &[u32], wave: WaveSize) -> Vec<Instruction> {
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(instruction) = decode_at(code, index, wave) {
        index += instruction.words.len().max(1);
        out.push(instruction);
    }
    out
}

/// Decodes the instructions of `code` that are reachable from its first
/// word. This never fails: instructions that cannot be named are kept and
/// stepped over, and anything that looks wrong is counted in the result.
pub fn walk(code: &[u32], wave: WaveSize) -> Walk {
    let mut found: BTreeMap<usize, Instruction> = BTreeMap::new();
    // Which words belong to an instruction already decoded.
    let mut taken = vec![false; code.len()];
    let mut pending = vec![0usize];
    let mut stray_branches = 0;
    let mut runs_off_end = code.is_empty();

    // Checks a branch target and turns it into a word index.
    let target_index = |target: i64| -> Option<usize> {
        let index = usize::try_from(target).ok()? / 4;
        (target % 4 == 0 && index < code.len()).then_some(index)
    };

    while let Some(mut index) = pending.pop() {
        loop {
            if found.contains_key(&index) {
                break;
            }
            let Some(instruction) = decode_at(code, index, wave) else {
                runs_off_end = true;
                break;
            };
            let length = instruction.words.len();
            if taken[index..index + length].iter().any(|&word| word) {
                // The path entered the middle of an instruction, or runs
                // into the start of one from a different alignment.
                stray_branches += 1;
                break;
            }
            taken[index..index + length].fill(true);
            let next = index + length;
            let truncated = instruction.truncated;
            let ends = instruction.ends_block();
            let target = instruction.branch_target();
            let far = if instruction.mnemonic == Some("s_setpc_b64") {
                far_jump(&found, &instruction)
            } else {
                None
            };
            found.insert(index, instruction);

            if truncated {
                runs_off_end = true;
                break;
            }
            // Where a branch, call or recognised far jump goes.
            let jump = target.or(far).map(target_index);
            match (ends, jump) {
                (true, Some(Some(to))) => index = to,
                (true, Some(None)) => {
                    stray_branches += 1;
                    break;
                }
                (true, None) => break,
                (false, Some(to)) => {
                    match to {
                        Some(to) => pending.push(to),
                        None => stray_branches += 1,
                    }
                    index = next;
                }
                (false, None) => index = next,
            }
        }
    }

    let unreached_words = taken.iter().filter(|&&word| !word).count();
    Walk {
        instructions: found.into_values().collect(),
        unreached_words,
        stray_branches,
        runs_off_end,
    }
}

/// Recognises the far-jump sequence that ends in the register jump `jump`
/// and returns the byte offset it goes to. `found` holds the instructions
/// decoded so far by word index.
fn far_jump(found: &BTreeMap<usize, Instruction>, jump: &Instruction) -> Option<i64> {
    let Some(Operand::Scalar {
        first: register, ..
    }) = jump.arg(Role::Source)
    else {
        return None;
    };
    let is_register = |operand: Option<Operand>| matches!(operand, Some(Operand::Scalar { first, .. }) if first == register);
    let mut distance: Option<i64> = None;
    let mut at = jump.offset;
    for _ in 0..FAR_JUMP_REACH {
        // The instruction that ends exactly where the last one starts.
        let (_, previous) = found.range(..(at / 4) as usize).next_back()?;
        if previous.end() != at {
            return None;
        }
        at = previous.offset;
        match (previous.format, previous.mnemonic) {
            (Format::Sop2, Some(name @ ("s_add_u32" | "s_sub_u32"))) if distance.is_none() => {
                let mut sources = previous.sources();
                let (first, second) = (sources.next(), sources.next());
                if let (true, true, Some(Operand::Literal(word))) = (
                    is_register(previous.arg(Role::Result)),
                    is_register(first),
                    second,
                ) {
                    // The literal is the low half of a signed 64-bit
                    // distance, so a backward jump adds a negative number.
                    let literal = i64::from(word as i32);
                    distance = Some(if name == "s_add_u32" {
                        literal
                    } else {
                        -literal
                    });
                }
            }
            (Format::Sop1, Some("s_getpc_b64")) if is_register(previous.arg(Role::Result)) => {
                // The register holds the address of the instruction after
                // the one that read the program counter.
                return distance.map(|distance| i64::from(previous.end()) + distance);
            }
            _ => {}
        }
    }
    None
}
