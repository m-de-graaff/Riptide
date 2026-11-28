//! A summary of how well a shader's machine code decodes.
//!
//! The decoder has to be checked against the real title, and none of the
//! title's data may be copied out of it. [`summarize`] is the answer: the
//! runtime calls it on every shader it meets and gets back counts only (how
//! many words, how many instructions, how many of them unknown, how often
//! each mnemonic occurs), which can be logged and compared freely.

use std::collections::BTreeMap;

use crate::Error;
use crate::container::xbox::{HardwareStage, Program};
use crate::container::{Container, ShaderKind};
use crate::gfx10::{Format, WaveSize, walk};

/// How many unknown instructions a summary describes one by one.
pub const UNKNOWN_SAMPLE_LIMIT: usize = 8;

/// One instruction the decoder could not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSample {
    /// The hardware stage of the program it was found in.
    pub stage: HardwareStage,
    /// The byte offset of the instruction in its program.
    pub offset: u32,
    /// The format the first word selected; [`Format::Unknown`] if none.
    pub format: Format,
    /// The opcode number within that format.
    pub opcode: u16,
    /// The instruction's first word.
    pub word: u32,
    /// Whether the instruction was cut off by the end of the code, which
    /// says the walk went wrong earlier and not that the opcode is unknown.
    pub truncated: bool,
}

/// The counts for one program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramSummary {
    /// The hardware stage the program runs as.
    pub stage: HardwareStage,
    /// The wave size the program was decoded for.
    pub wave: WaveSize,
    /// How many 32-bit words the program's code has, data included.
    pub code_words: usize,
    /// How many instructions were reached and decoded.
    pub instructions: usize,
    /// How many of those could not be named.
    pub unknown: usize,
    /// The first few instructions that could not be named.
    pub unknown_samples: Vec<UnknownSample>,
    /// How many words no reached instruction covers: data after the program,
    /// or code only reachable through a register jump.
    pub unreached_words: usize,
    /// How many branches went outside the code or into the middle of an
    /// instruction. Anything but 0 means the decode is wrong somewhere.
    pub stray_branches: usize,
    /// Whether a path ran past the last word. `true` means the decode is
    /// wrong somewhere.
    pub runs_off_end: bool,
    /// How many times each mnemonic occurs.
    pub mnemonics: BTreeMap<&'static str, u32>,
}

/// The counts for one shader container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The shader stage, as the program header of the `DXIL` part states it.
    pub kind: ShaderKind,
    /// The counts of each program in the container. Most shaders have one.
    pub programs: Vec<ProgramSummary>,
    /// How many code words all programs have together.
    pub code_words: usize,
    /// How many instructions were decoded in all programs together.
    pub instructions: usize,
    /// How many of those could not be named.
    pub unknown: usize,
    /// The first few instructions that could not be named, over all programs.
    pub unknown_samples: Vec<UnknownSample>,
    /// How many times each mnemonic occurs in all programs together.
    pub mnemonics: BTreeMap<&'static str, u32>,
}

/// Decodes one program by following its control flow and counts what was
/// found.
pub fn summarize_program(program: &Program) -> ProgramSummary {
    let walked = walk(&program.code, program.wave);
    let mut mnemonics = BTreeMap::new();
    let mut unknown = 0;
    let mut unknown_samples = Vec::new();
    for instruction in &walked.instructions {
        match instruction.mnemonic {
            Some(name) => *mnemonics.entry(name).or_insert(0) += 1,
            None => {
                unknown += 1;
                if unknown_samples.len() < UNKNOWN_SAMPLE_LIMIT {
                    unknown_samples.push(UnknownSample {
                        stage: program.stage,
                        offset: instruction.offset,
                        format: instruction.format,
                        opcode: instruction.opcode,
                        word: instruction.words.first().copied().unwrap_or(0),
                        truncated: instruction.truncated,
                    });
                }
            }
        }
    }
    ProgramSummary {
        stage: program.stage,
        wave: program.wave,
        code_words: program.code.len(),
        instructions: walked.instructions.len(),
        unknown,
        unknown_samples,
        unreached_words: walked.unreached_words,
        stray_branches: walked.stray_branches,
        runs_off_end: walked.runs_off_end,
        mnemonics,
    }
}

/// Reads a shader container, finds its machine code, decodes every program
/// in it and returns the counts.
///
/// Fails with [`Error::NoMachineCode`] if the container has no `DXIL` part
/// or the part holds no machine code (the PC layout), and with the reading
/// error if the container is damaged.
pub fn summarize(container: &[u8]) -> Result<Summary, Error> {
    let container = Container::parse(container)?;
    let dxil = container.xbox_dxil()?.ok_or(Error::NoMachineCode)?;
    let mut summary = Summary {
        kind: dxil.header.kind,
        programs: Vec::new(),
        code_words: 0,
        instructions: 0,
        unknown: 0,
        unknown_samples: Vec::new(),
        mnemonics: BTreeMap::new(),
    };
    for machine_code in dxil.machine_code()? {
        for program in &machine_code.programs {
            let counts = summarize_program(program);
            summary.code_words += counts.code_words;
            summary.instructions += counts.instructions;
            summary.unknown += counts.unknown;
            let room = UNKNOWN_SAMPLE_LIMIT - summary.unknown_samples.len();
            summary
                .unknown_samples
                .extend(counts.unknown_samples.iter().take(room));
            for (&name, &count) in &counts.mnemonics {
                *summary.mnemonics.entry(name).or_insert(0) += count;
            }
            summary.programs.push(counts);
        }
    }
    if summary.programs.is_empty() {
        return Err(Error::NoMachineCode);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::testing::{
        SMALL_PROGRAM, TestProgram, dxbc, dxil_part, machine_code, packed, pixel_container,
    };

    #[test]
    fn summarizes_the_synthetic_pixel_shader() {
        for pack in [false, true] {
            let summary = summarize(&pixel_container(pack)).unwrap();
            assert_eq!(summary.kind, ShaderKind::Pixel);
            assert_eq!(summary.code_words, SMALL_PROGRAM.len());
            assert_eq!(summary.instructions, 3);
            assert_eq!(summary.unknown, 0);
            assert!(summary.unknown_samples.is_empty());
            let counts: Vec<(&str, u32)> = summary
                .mnemonics
                .iter()
                .map(|(&name, &count)| (name, count))
                .collect();
            assert_eq!(
                counts,
                [("s_cbranch_execz", 1), ("s_endpgm", 1), ("s_mov_b32", 1)]
            );
            assert_eq!(summary.programs.len(), 1);
            let program = &summary.programs[0];
            assert_eq!(program.stage, HardwareStage::Pixel);
            assert_eq!(program.wave, WaveSize::Wave64);
            assert_eq!(program.unreached_words, 1);
            assert_eq!(program.stray_branches, 0);
            assert!(!program.runs_off_end);
        }
    }

    #[test]
    fn counts_unknown_instructions_and_adds_programs_up() {
        // A vertex program with two unassigned opcodes and a repeated
        // mnemonic, and a compute program of 32 lanes with twelve unknown
        // instructions: more than a summary lists one by one.
        let vertex = [
            0x8600_0000, // SOP2 opcode 0x0C, unassigned
            0xBF80_0000, // s_nop
            0xBF80_0000, // s_nop
            0xD7FF_0000, // VOP3 opcode 0x3FF, unassigned
            0x0000_0000,
            0xBF81_0000, // s_endpgm
        ];
        let mut compute = vec![0xC000_0000u32; 12];
        compute.push(0xBF81_0000);
        let structure = machine_code(&[
            TestProgram::new(0x28, false, &vertex),
            TestProgram::new(0x60, true, &compute),
        ]);
        let bytes = dxbc(&[(b"DXIL", dxil_part(1, &[(0x0D, packed(&structure))]))]);
        let summary = summarize(&bytes).unwrap();
        assert_eq!(summary.kind, ShaderKind::Vertex);
        assert_eq!(summary.programs.len(), 2);
        assert_eq!(summary.code_words, 6 + 13);
        assert_eq!(summary.instructions, 5 + 13);
        assert_eq!(summary.unknown, 2 + 12);
        assert_eq!(summary.mnemonics["s_nop"], 2);
        assert_eq!(summary.mnemonics["s_endpgm"], 2);
        assert_eq!(summary.mnemonics.len(), 2);

        assert_eq!(summary.unknown_samples.len(), UNKNOWN_SAMPLE_LIMIT);
        assert_eq!(
            summary.unknown_samples[0],
            UnknownSample {
                stage: HardwareStage::Vertex,
                offset: 0,
                format: Format::Sop2,
                opcode: 0x0C,
                word: 0x8600_0000,
                truncated: false,
            }
        );
        assert_eq!(
            summary.unknown_samples[1],
            UnknownSample {
                stage: HardwareStage::Vertex,
                offset: 12,
                format: Format::Vop3a,
                opcode: 0x3FF,
                word: 0xD7FF_0000,
                truncated: false,
            }
        );
        assert_eq!(summary.unknown_samples[2].stage, HardwareStage::Compute);
        assert_eq!(summary.unknown_samples[2].format, Format::Unknown);

        let vertex = &summary.programs[0];
        assert_eq!((vertex.instructions, vertex.unknown), (5, 2));
        let compute = &summary.programs[1];
        assert_eq!(compute.wave, WaveSize::Wave32);
        assert_eq!((compute.instructions, compute.unknown), (13, 12));
        assert_eq!(compute.unknown_samples.len(), UNKNOWN_SAMPLE_LIMIT);
    }

    #[test]
    fn a_container_that_cannot_be_read_gives_its_error() {
        assert_eq!(summarize(b"not a shader"), Err(Error::NotDxbc));
        let mut bytes = pixel_container(true);
        let last = bytes.len() - 20;
        bytes.truncate(last);
        assert!(summarize(&bytes).is_err());
    }
}
