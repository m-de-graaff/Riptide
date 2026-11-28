//! Tests of the lift, on synthetic programs only.
//!
//! Every program here is assembled by [`asm`] from the public field layouts
//! of the instruction set; no word comes from a real title. Most tests run
//! a program twice: through the lift and the reference interpreter, and
//! through the wave-level interpreter of the machine code
//! ([`crate::interp::machine`]), with hosts that answer alike, and compare
//! everything that came out ([`run_both`]). The inputs differ per lane and
//! include NaNs, infinities, both zeroes and plain bit patterns.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::container::stage::{
    ComputeData, ExportPatch, PixelData, PixelInput, StageData, StageOutput, VertexData,
    VertexInput,
};
use crate::container::xbox::{HardwareStage, Program, RootParameter};
use crate::gfx10::WaveSize;
use crate::interp::machine::run_machine;
use crate::interp::{Host, run_wave};
use crate::ir::cfg::ControlFlow;
use crate::ir::{
    ExportTarget, Function, ImageKind, ImageOp, Input, Level, Op, Sampler, Value, VertexTerm,
};
use crate::lift::lift;

/// A small assembler for the tests.
pub(crate) mod asm {
    use crate::gfx10::{Format, WaveSize, decode_at, number_of};

    /// An operand of an assembled instruction.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub(crate) enum O {
        /// Scalar register.
        S(u8),
        /// Vector register.
        V(u8),
        /// Inline integer constant, -16 to 64.
        K(i8),
        /// A 32-bit literal after the instruction.
        Lit(u32),
        /// `EXEC` (its low half in a 32-bit operation).
        Exec,
        /// The high half of `EXEC`.
        ExecHigh,
        /// `VCC`.
        Vcc,
        /// The inline constant 1.0.
        One,
    }

    impl O {
        /// The operand's code in a source field.
        pub(crate) fn code(self) -> u32 {
            match self {
                O::S(register) => u32::from(register),
                O::V(register) => 256 + u32::from(register),
                O::K(value) if value >= 0 => 128 + value as u32,
                O::K(value) => 192 + u32::from(value.unsigned_abs()),
                O::Lit(_) => 255,
                O::Exec => 126,
                O::ExecHigh => 127,
                O::Vcc => 106,
                O::One => 242,
            }
        }

        fn literal(self) -> Option<u32> {
            match self {
                O::Lit(word) => Some(word),
                _ => None,
            }
        }
    }

    /// The opcode number of a mnemonic.
    fn op(format: Format, name: &str) -> u32 {
        u32::from(number_of(format, name).unwrap_or_else(|| panic!("no opcode {name}")))
    }

    /// Checks that the assembled words decode to the mnemonic meant, and
    /// returns them.
    fn checked(name: &str, words: Vec<u32>) -> Vec<u32> {
        let decoded = decode_at(&words, 0, WaveSize::Wave64).unwrap();
        assert_eq!(decoded.mnemonic, Some(name), "assembled {words:x?}");
        assert_eq!(decoded.words.len(), words.len(), "{name}");
        words
    }

    /// Appends the literal of the first operand that has one.
    fn with_literal(mut words: Vec<u32>, operands: &[O]) -> Vec<u32> {
        if let Some(literal) = operands.iter().find_map(|operand| operand.literal()) {
            words.push(literal);
        }
        words
    }

    pub(crate) fn sop2(name: &str, target: O, a: O, b: O) -> Vec<u32> {
        let word =
            2 << 30 | op(Format::Sop2, name) << 23 | target.code() << 16 | b.code() << 8 | a.code();
        checked(name, with_literal(vec![word], &[a, b]))
    }

    pub(crate) fn sop1(name: &str, target: O, a: O) -> Vec<u32> {
        let word = 0x17D << 23 | target.code() << 16 | op(Format::Sop1, name) << 8 | a.code();
        checked(name, with_literal(vec![word], &[a]))
    }

    pub(crate) fn sopc(name: &str, a: O, b: O) -> Vec<u32> {
        let word = 0x17E << 23 | op(Format::Sopc, name) << 16 | b.code() << 8 | a.code();
        checked(name, with_literal(vec![word], &[a, b]))
    }

    pub(crate) fn sopk(name: &str, register: O, immediate: u16) -> Vec<u32> {
        let word =
            0xB << 28 | op(Format::Sopk, name) << 23 | register.code() << 16 | u32::from(immediate);
        checked(name, vec![word])
    }

    /// `s_setreg_imm32_b32`: a field of a hardware register and its value.
    pub(crate) fn setreg(register: u16, offset: u16, size: u16, value: u32) -> Vec<u32> {
        let name = "s_setreg_imm32_b32";
        let immediate = u32::from(register | offset << 6 | (size - 1) << 11);
        let word = 0xB << 28 | op(Format::Sopk, name) << 23 | immediate;
        checked(name, vec![word, value])
    }

    pub(crate) fn sopp(name: &str, immediate: u16) -> Vec<u32> {
        let word = 0x17F << 23 | op(Format::Sopp, name) << 16 | u32::from(immediate);
        checked(name, vec![word])
    }

    pub(crate) fn vop2(name: &str, target: u8, a: O, b: u8) -> Vec<u32> {
        let word =
            op(Format::Vop2, name) << 25 | u32::from(target) << 17 | u32::from(b) << 9 | a.code();
        checked(name, with_literal(vec![word], &[a]))
    }

    pub(crate) fn vop1(name: &str, target: u8, a: O) -> Vec<u32> {
        let word = 0x3F << 25 | u32::from(target) << 17 | op(Format::Vop1, name) << 9 | a.code();
        checked(name, with_literal(vec![word], &[a]))
    }

    /// A one-source operation whose result is a scalar register.
    pub(crate) fn vop1_to_scalar(name: &str, target: u8, a: O) -> Vec<u32> {
        vop1(name, target, a)
    }

    pub(crate) fn vopc(name: &str, a: O, b: u8) -> Vec<u32> {
        let word = 0x3E << 25 | op(Format::Vopc, name) << 17 | u32::from(b) << 9 | a.code();
        checked(name, with_literal(vec![word], &[a]))
    }

    /// The three-source encoding. `target` is a vector register number, or
    /// a scalar register code for a comparison. `absolute` and `negate`
    /// have a bit per source.
    pub(crate) fn vop3(
        name: &str,
        target: u32,
        sources: [O; 3],
        absolute: u32,
        negate: u32,
    ) -> Vec<u32> {
        let first = 0x35 << 26 | op(Format::Vop3a, name) << 16 | absolute << 8 | target;
        let second =
            negate << 29 | sources[2].code() << 18 | sources[1].code() << 9 | sources[0].code();
        checked(name, with_literal(vec![first, second], &sources))
    }

    /// The three-source encoding of an addition with a carry out into the
    /// scalar registers at `carry`.
    pub(crate) fn vop3_carry(name: &str, target: u8, carry: O, sources: [O; 3]) -> Vec<u32> {
        vop3(name, u32::from(target), sources, carry.code(), 0)
    }

    /// A two-source operation with a sub-register word: which part of each
    /// source is read (with or without sign extension) and which part of
    /// the result is written, with `unused` saying what becomes of the rest.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn vop2_sub(
        name: &str,
        target: u8,
        a: u8,
        b: u8,
        result_select: u32,
        unused: u32,
        selects: [u32; 2],
        sign_extend: [bool; 2],
    ) -> Vec<u32> {
        let first =
            op(Format::Vop2, name) << 25 | u32::from(target) << 17 | u32::from(b) << 9 | 0xF9;
        let second = u32::from(a)
            | result_select << 8
            | unused << 11
            | selects[0] << 16
            | u32::from(sign_extend[0]) << 19
            | selects[1] << 24
            | u32::from(sign_extend[1]) << 27;
        checked(name, vec![first, second])
    }

    /// A move through a permutation of the lanes of each quad.
    pub(crate) fn mov_quad(
        target: u8,
        source: u8,
        control: u32,
        fetch: bool,
        bound: bool,
    ) -> Vec<u32> {
        let name = "v_mov_b32";
        let first = 0x3F << 25 | u32::from(target) << 17 | op(Format::Vop1, name) << 9 | 0xFA;
        let second = u32::from(source)
            | control << 8
            | u32::from(fetch) << 18
            | u32::from(bound) << 19
            | 0xFF << 24;
        checked(name, vec![first, second])
    }

    /// A two-source operation whose first source each lane reads from
    /// another lane of its group of eight: `lanes[n]` is where lane n
    /// reads.
    pub(crate) fn vop2_eight(name: &str, target: u8, a: u8, b: u8, lanes: [u32; 8]) -> Vec<u32> {
        let first =
            op(Format::Vop2, name) << 25 | u32::from(target) << 17 | u32::from(b) << 9 | 0xE9;
        let mut second = u32::from(a);
        for (lane, pick) in lanes.into_iter().enumerate() {
            second |= pick << (8 + 3 * lane);
        }
        checked(name, vec![first, second])
    }

    /// A two-source operation whose first source each lane reads from
    /// another lane of its row of sixteen, chosen by a control number.
    /// `bound` makes a read that finds nothing give zero.
    pub(crate) fn vop2_rows(
        name: &str,
        target: u8,
        a: u8,
        b: u8,
        control: u32,
        bound: bool,
    ) -> Vec<u32> {
        let first =
            op(Format::Vop2, name) << 25 | u32::from(target) << 17 | u32::from(b) << 9 | 0xFA;
        let second = u32::from(a) | control << 8 | u32::from(bound) << 19 | 0xFF << 24;
        checked(name, vec![first, second])
    }

    /// `v_permlanex16_b32`: each lane reads a lane of the other row of its
    /// half of the wave, picked by four bits of the two selectors. Bit 0 of
    /// `select` fetches lanes that are not live.
    pub(crate) fn across_rows(target: u8, source: u8, selectors: i8, select: u32) -> Vec<u32> {
        vop3(
            "v_permlanex16_b32",
            u32::from(target),
            [O::V(source), O::K(selectors), O::K(selectors)],
            select << 3,
            0,
        )
    }

    /// `v_readlane_b32`: one lane's word into a scalar register.
    pub(crate) fn read_lane(target: u8, source: u8, lane: i8) -> Vec<u32> {
        vop3(
            "v_readlane_b32",
            u32::from(target),
            [O::V(source), O::K(lane), O::K(0)],
            0,
            0,
        )
    }

    /// An operation on the group-shared memory: the result, address and two
    /// data registers, and the two offset bytes.
    pub(crate) fn ds(
        name: &str,
        result: u8,
        address: u8,
        data: [u8; 2],
        offsets: [u8; 2],
    ) -> Vec<u32> {
        let first = 0x36 << 26
            | op(Format::Ds, name) << 18
            | u32::from(offsets[1]) << 8
            | u32::from(offsets[0]);
        let second = u32::from(result) << 24
            | u32::from(data[1]) << 16
            | u32::from(data[0]) << 8
            | u32::from(address);
        checked(name, vec![first, second])
    }

    /// Sets the bit of a buffer or by-address operation that makes an
    /// atomic one return the word as it was.
    pub(crate) fn returning(mut words: Vec<u32>) -> Vec<u32> {
        // Buffer operations have it at bit 14, the others at bit 16.
        let buffer = words[0] >> 26 == 0x38;
        words[0] |= 1 << if buffer { 14 } else { 16 };
        words
    }

    pub(crate) fn smem(
        name: &str,
        data: u8,
        base: u8,
        offset: u32,
        register: Option<u8>,
    ) -> Vec<u32> {
        let first =
            0x3D << 26 | op(Format::Smem, name) << 18 | u32::from(data) << 6 | u32::from(base / 2);
        let second = u32::from(register.unwrap_or(125)) << 25 | offset;
        checked(name, vec![first, second])
    }

    /// An untyped buffer access: data and address registers, the descriptor
    /// in scalar registers, a constant offset, and which of the address
    /// registers are an index and an offset.
    pub(crate) fn mubuf(
        name: &str,
        data: u8,
        address: u8,
        descriptor: u8,
        offset: u32,
        index_enable: bool,
        offset_enable: bool,
    ) -> Vec<u32> {
        let code = op(Format::Mubuf, name);
        let first = 0x38 << 26
            | (code >> 7) << 25
            | (code & 0x7F) << 18
            | u32::from(index_enable) << 13
            | u32::from(offset_enable) << 12
            | offset;
        let second =
            128 << 24 | u32::from(descriptor / 4) << 16 | u32::from(data) << 8 | u32::from(address);
        checked(name, vec![first, second])
    }

    /// A typed buffer access with the data format in the instruction.
    pub(crate) fn mtbuf(
        name: &str,
        format: u32,
        data: u8,
        address: u8,
        descriptor: u8,
        offset: u32,
    ) -> Vec<u32> {
        let code = op(Format::Mtbuf, name);
        let first = 0x3A << 26 | format << 19 | (code & 7) << 16 | 1 << 13 | offset;
        let second = 128 << 24
            | (code >> 3) << 21
            | u32::from(descriptor / 4) << 16
            | u32::from(data) << 8
            | u32::from(address);
        checked(name, vec![first, second])
    }

    /// An access by address: a scalar base pair and a vector offset.
    pub(crate) fn global(
        name: &str,
        result: u8,
        address: u8,
        data: u8,
        base: u8,
        offset: u32,
    ) -> Vec<u32> {
        let first = 0x37 << 26 | op(Format::Global, name) << 18 | 2 << 14 | offset;
        let second = u32::from(result) << 24
            | u32::from(base) << 16
            | u32::from(data) << 8
            | u32::from(address);
        checked(name, vec![first, second])
    }

    pub(crate) fn swizzle(target: u8, source: u8, offset: u32) -> Vec<u32> {
        let name = "ds_swizzle_b32";
        let first = 0x36 << 26 | op(Format::Ds, name) << 18 | offset;
        let second = u32::from(target) << 24 | u32::from(source);
        checked(name, vec![first, second])
    }

    pub(crate) fn exp(target: u32, registers: [u8; 4], done: bool) -> Vec<u32> {
        let first = 0x3E << 26 | u32::from(done) << 11 | target << 4 | 0xF;
        let second = u32::from_le_bytes(registers);
        checked("exp", vec![first, second])
    }

    pub(crate) fn interp(
        name: &str,
        target: u8,
        source: u8,
        attribute: u32,
        channel: u32,
    ) -> Vec<u32> {
        let word = 0x32 << 26
            | u32::from(target) << 18
            | op(Format::Vintrp, name) << 16
            | attribute << 10
            | channel << 8
            | u32::from(source);
        checked(name, vec![word])
    }

    /// An image operation: data and first address register, the image
    /// descriptor and the sampler descriptor in scalar registers.
    pub(crate) fn mimg(
        name: &str,
        channels: u32,
        dimension: u32,
        data: u8,
        address: u8,
        image: u8,
        sampler: u8,
    ) -> Vec<u32> {
        let code = op(Format::Mimg, name);
        let first = 0x3C << 26 | (code & 0x7F) << 18 | channels << 8 | dimension << 3 | code >> 7;
        let second = u32::from(sampler / 4) << 21
            | u32::from(image / 4) << 16
            | u32::from(data) << 8
            | u32::from(address);
        checked(name, vec![first, second])
    }

    /// Marks an image operation as taking 16-bit addresses.
    pub(crate) fn with_short_addresses(mut words: Vec<u32>) -> Vec<u32> {
        words[1] |= 1 << 30;
        words
    }

    /// A program under assembly: instructions, labels, and branches to
    /// labels.
    #[derive(Default)]
    pub(crate) struct Code {
        words: Vec<u32>,
        labels: Vec<(&'static str, usize)>,
        branches: Vec<(usize, &'static str)>,
    }

    impl Code {
        pub(crate) fn new() -> Code {
            Code::default()
        }

        /// Appends an instruction.
        pub(crate) fn i(mut self, words: Vec<u32>) -> Code {
            self.words.extend(words);
            self
        }

        /// Names the place of the next instruction.
        pub(crate) fn label(mut self, name: &'static str) -> Code {
            self.labels.push((name, self.words.len()));
            self
        }

        /// Appends a branch to a label.
        pub(crate) fn branch(mut self, name: &str, label: &'static str) -> Code {
            self.branches.push((self.words.len(), label));
            self.words.extend(sopp(name, 0));
            self
        }

        /// Ends the program and fills in the branch distances.
        pub(crate) fn end(mut self) -> Vec<u32> {
            self.words.extend(sopp("s_endpgm", 0));
            for &(at, label) in &self.branches {
                let target = self
                    .labels
                    .iter()
                    .find(|(name, _)| *name == label)
                    .unwrap_or_else(|| panic!("no label {label}"))
                    .1;
                let distance = target as i64 - (at as i64 + 1);
                self.words[at] |= u32::from(distance as i16 as u16);
            }
            self.words
        }
    }
}

use asm::{Code, O};

/// A scrambling of a number, for inputs and memory that differ everywhere.
fn mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A float between -8 and 8 in steps of 1/256.
fn tidy_float(seed: u64) -> u32 {
    ((mix(seed) % 4096) as f32 / 256.0 - 8.0).to_bits()
}

/// A word for a lane: by turns a plain bit pattern, a tidy float, a small
/// integer and a float that needs care.
fn lane_word(lane: usize, salt: u64) -> u32 {
    let seed = (lane as u64) << 8 | salt;
    let hash = mix(seed);
    match lane % 4 {
        0 => hash as u32,
        1 => tidy_float(seed),
        2 => (hash % 37) as u32,
        _ => {
            let special = [
                0.0f32,
                -0.0,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NAN,
                1.0,
                -0.5,
                1e-40,
            ];
            special[(hash % 8) as usize].to_bits()
        }
    }
}

/// The exports of one lane, in the order the lane made them.
pub(crate) type LaneExports = Vec<(ExportTarget, [Option<u32>; 4])>;

/// Everything a run put out, in an order that does not depend on how the
/// lanes were scheduled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Outcome {
    /// Exports by lane, in the order each lane made them.
    pub exports: BTreeMap<usize, LaneExports>,
    /// The lanes discarded.
    pub discards: BTreeSet<usize>,
    /// The bytes stored to title memory.
    pub stores: BTreeMap<u64, u8>,
    /// Image stores by lane: a digest of the access.
    pub image_stores: BTreeMap<usize, Vec<u64>>,
}

/// A host for tests: inputs and memory are made up but fixed, and
/// everything that comes out is recorded.
pub(crate) struct TestHost {
    /// The user-data words by register.
    pub user: [u32; 64],
    /// Memory that was set up or stored; other addresses read a fixed
    /// scramble of the address.
    pub memory: HashMap<u64, u8>,
    /// What came out.
    pub outcome: Outcome,
}

impl TestHost {
    pub(crate) fn new() -> TestHost {
        let mut user = [0u32; 64];
        for (register, word) in user.iter_mut().enumerate() {
            *word = mix(0xABCD + register as u64) as u32;
        }
        TestHost {
            user,
            memory: HashMap::new(),
            outcome: Outcome::default(),
        }
    }

    /// Puts 32-bit words into memory.
    pub(crate) fn write_words(&mut self, address: u64, words: &[u32]) {
        for (index, word) in words.iter().enumerate() {
            for (byte, value) in word.to_le_bytes().into_iter().enumerate() {
                self.memory
                    .insert(address + 4 * index as u64 + byte as u64, value);
            }
        }
    }

    /// A digest of an image access with its operands filled in.
    fn digest(access: &ImageOp) -> u64 {
        let word = |value: Value| match value {
            Value::Word(word) => u64::from(word),
            Value::Flag(flag) => u64::from(flag),
            Value::Temp(_) => panic!("an image access with an operand left open"),
        };
        let mut parts: Vec<u64> = vec![
            access.dimension as u64,
            u64::from(access.compare),
            u64::from(access.unnormalized),
            word(access.texture),
            word(access.format),
        ];
        parts.push(match &access.kind {
            ImageKind::Sample => 1,
            ImageKind::Gather { channel } => 2 + u64::from(*channel),
            ImageKind::Load => 10,
            ImageKind::Store { .. } => 11,
            ImageKind::Info => 12,
            ImageKind::QueryLevel => 13,
        });
        // A sampler fixed by the root signature and one from the heap are
        // told apart by the lift only; both interpreters must agree, so the
        // digest takes the bits the two have in common.
        parts.push(match access.sampler {
            None => u64::MAX,
            Some(Sampler::Heap(index)) => word(index),
            Some(Sampler::Immediate(words)) => u64::from(words[3] & 0xFFF),
        });
        for value in [
            access.offset,
            access.bias,
            access.reference,
            access.level_clamp,
            access.mip,
        ] {
            parts.push(value.map_or(u64::MAX - 1, word));
        }
        parts.extend(access.coordinates.iter().map(|&value| word(value)));
        match &access.level {
            Level::Implicit => parts.push(100),
            Level::Zero => parts.push(101),
            Level::Explicit(level) => parts.extend([102, word(*level)]),
            Level::Gradients(dx, dy) => {
                parts.push(103);
                parts.extend(dx.iter().chain(dy).map(|&value| word(value)));
            }
        }
        if let ImageKind::Store { values, .. } = &access.kind {
            parts.extend(values.iter().map(|&value| word(value)));
        }
        parts.into_iter().fold(7, |digest, part| mix(digest ^ part))
    }
}

impl Host for TestHost {
    fn input(&mut self, lane: usize, input: Input) -> u32 {
        let vertex_term = |attribute: u8, channel: u8, term: u64| {
            tidy_float(0x5000 + (u64::from(attribute) << 8 | u64::from(channel) << 4 | term))
        };
        match input {
            Input::UserData(register) => self.user[usize::from(register) % 64],
            Input::VertexId => lane_word(lane, 1),
            Input::InstanceId => lane_word(lane, 2),
            Input::VertexAttribute { row, channel } => {
                lane_word(lane, 16 + u64::from(row) * 4 + u64::from(channel))
            }
            Input::Weight { weights, second } => {
                tidy_float((lane as u64) << 8 | 0x80 | (weights as u64) << 1 | u64::from(second))
            }
            Input::Attribute {
                attribute,
                channel,
                term,
            } => vertex_term(attribute, channel, term as u64),
            Input::Interpolated {
                attribute: number,
                channel,
                weights,
            } => {
                // What the two halves of the hardware's evaluation give.
                let term =
                    |term: VertexTerm| f32::from_bits(vertex_term(number, channel, term as u64));
                let weight = |host: &mut Self, second| {
                    f32::from_bits(host.input(lane, Input::Weight { weights, second }))
                };
                let first =
                    term(VertexTerm::P10).mul_add(weight(self, false), term(VertexTerm::P0));
                term(VertexTerm::P20)
                    .mul_add(weight(self, true), first)
                    .to_bits()
            }
            Input::Position(channel) => tidy_float((lane as u64) << 8 | 0x40 | u64::from(channel)),
            Input::FrontFace => (lane % 2) as u32,
            Input::Ancillary => lane_word(lane, 3),
            Input::SampleCoverage => 1,
            Input::FixedPosition => lane_word(lane, 4),
            Input::ThreadId(axis) => lane as u32 + u32::from(axis),
            Input::GroupId(axis) => 5 + u32::from(axis),
        }
    }

    fn load(&mut self, address: u64, width: u8) -> u32 {
        let mut value = 0u32;
        for byte in 0..u64::from(width / 8) {
            let at = address.wrapping_add(byte);
            let stored = self
                .memory
                .get(&at)
                .copied()
                .unwrap_or((mix(at) >> 13) as u8);
            value |= u32::from(stored) << (8 * byte);
        }
        value
    }

    fn store(&mut self, address: u64, width: u8, value: u32) {
        for byte in 0..u64::from(width / 8) {
            let at = address.wrapping_add(byte);
            let stored = (value >> (8 * byte)) as u8;
            self.memory.insert(at, stored);
            self.outcome.stores.insert(at, stored);
        }
    }

    fn image(&mut self, lane: usize, access: &ImageOp) -> [u32; 4] {
        let digest = Self::digest(access);
        if matches!(access.kind, ImageKind::Store { .. }) {
            self.outcome
                .image_stores
                .entry(lane)
                .or_default()
                .push(digest);
            return [0; 4];
        }
        [0, 1, 2, 3].map(|channel| tidy_float(digest ^ channel))
    }

    fn export(&mut self, lane: usize, target: ExportTarget, values: [Option<u32>; 4]) {
        self.outcome
            .exports
            .entry(lane)
            .or_default()
            .push((target, values));
    }

    fn discard(&mut self, lane: usize) {
        self.outcome.discards.insert(lane);
    }
}

/// A program of the given stage with user data in the registers named.
pub(crate) fn program(
    stage: HardwareStage,
    wave: WaveSize,
    code: Vec<u32>,
    user: &[u8],
) -> Program {
    Program {
        stage,
        wave,
        flags: 0,
        vector_registers: 32,
        scalar_registers: 32,
        group_shared_bytes: 0,
        record_offset: 0,
        code_offset: 0,
        root_parameters: user
            .iter()
            .enumerate()
            .map(|(slot, &register)| RootParameter {
                root_type: 0,
                table_offset_words: 0,
                register_count: 1,
                base_register: 0,
                user_data: register,
                slot: slot as u8,
            })
            .collect(),
        used_resources: Vec::new(),
        code,
    }
}

/// The stage data of a vertex program without attributes.
pub(crate) fn vertex_data() -> StageData {
    StageData::Vertex(VertexData {
        inputs: Vec::new(),
        outputs: vec![StageOutput {
            usage: 6,
            row: 1,
            parameter: 0,
            row_again: 0,
            slot: 0,
            flags: 0,
        }],
    })
}

/// The stage data of a pixel program that is handed the perspective
/// weights at the pixel centre (in `v0` and `v1`) and has two attributes.
pub(crate) fn pixel_data() -> StageData {
    let input = |attribute: u8| PixelInput {
        usage: 6,
        row: attribute + 1,
        mask: 0xF,
        attribute,
        default: 0,
        second_row: 0,
        slot: attribute,
        flags: 2,
    };
    StageData::Pixel(PixelData {
        inputs: vec![input(0), input(1)],
        input_enable: 2,
        depth_control: 0x10,
        patch_words: 6,
        patches: Vec::new(),
    })
}

/// A vertex program over a 64-lane wave with user data in `s8` to `s11`.
fn vertex(code: Vec<u32>) -> Program {
    program(
        HardwareStage::Vertex,
        WaveSize::Wave64,
        code,
        &[8, 9, 10, 11],
    )
}

/// Lifts a program, checks the function's shape, and runs it and the
/// machine code with hosts set up alike by `prepare`. Returns the function
/// and what came out, which is the same from both.
pub(crate) fn run_both_with(
    program: &Program,
    data: &StageData,
    lanes: usize,
    prepare: impl Fn(&mut TestHost),
) -> (Function, Outcome) {
    let function = lift(program, data).unwrap_or_else(|error| panic!("{error}"));
    function.verify().unwrap();
    assert!(ControlFlow::of(&function).reducible);
    let mut lifted = TestHost::new();
    prepare(&mut lifted);
    run_wave(&function, lanes, &mut lifted).unwrap_or_else(|error| panic!("{error}\n{function}"));
    let mut direct = TestHost::new();
    prepare(&mut direct);
    run_machine(program, data, lanes, &mut direct).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        lifted.outcome, direct.outcome,
        "the lifted program and the machine code disagree\n{function}"
    );
    (function, lifted.outcome)
}

/// [`run_both_with`] without any set-up.
pub(crate) fn run_both(program: &Program, data: &StageData, lanes: usize) -> (Function, Outcome) {
    run_both_with(program, data, lanes, |_| {})
}

/// The usual end of a test program: position 0 from four registers.
fn export_position(registers: [u8; 4]) -> Vec<u32> {
    asm::exp(12, registers, true)
}

/// The words a lane exported to position 0.
fn position(outcome: &Outcome, lane: usize) -> [u32; 4] {
    let (target, values) = outcome.exports[&lane][0];
    assert_eq!(target, ExportTarget::Position(0));
    values.map(|value| value.unwrap())
}

#[test]
fn every_two_source_vector_operation_agrees_with_the_machine() {
    for name in [
        "v_add_f32",
        "v_sub_f32",
        "v_subrev_f32",
        "v_mul_f32",
        "v_mul_legacy_f32",
        "v_min_f32",
        "v_max_f32",
        "v_mac_f32",
        "v_fmac_f32",
        "v_add_nc_u32",
        "v_sub_nc_u32",
        "v_subrev_nc_u32",
        "v_and_b32",
        "v_or_b32",
        "v_xor_b32",
        "v_lshlrev_b32",
        "v_lshrrev_b32",
        "v_ashrrev_i32",
        "v_min_u32",
        "v_max_u32",
        "v_min_i32",
        "v_max_i32",
        "v_mul_u32_u24",
        "v_cvt_pkrtz_f16_f32",
    ] {
        let _ = name;
    }
    // The 16-bit shifts exist only in the three-source encoding.
    for name in ["v_lshrrev_b16", "v_lshlrev_b16"] {
        let code = Code::new()
            .i(asm::vop1("v_mov_b32", 5, O::V(3)))
            .i(asm::vop3(name, 5, [O::V(0), O::V(3), O::K(0)], 0, 0))
            .i(asm::vop3(name, 6, [O::K(5), O::V(0), O::K(0)], 0, 0))
            .i(export_position([5, 6, 0, 3]))
            .end();
        let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
        for lane in 0..64 {
            let (a, b) = (lane_word(lane, 1), lane_word(lane, 2));
            let shifted = if name == "v_lshrrev_b16" {
                (b & 0xFFFF) >> (a & 15)
            } else {
                (b << (a & 15)) & 0xFFFF
            };
            // The high half of the register stays as it was.
            assert_eq!(position(&outcome, lane)[0], b & 0xFFFF_0000 | shifted);
        }
    }
    for name in [
        "v_add_f32",
        "v_sub_f32",
        "v_subrev_f32",
        "v_mul_f32",
        "v_mul_legacy_f32",
        "v_min_f32",
        "v_max_f32",
        "v_mac_f32",
        "v_fmac_f32",
        "v_add_nc_u32",
        "v_sub_nc_u32",
        "v_subrev_nc_u32",
        "v_and_b32",
        "v_or_b32",
        "v_xor_b32",
        "v_lshlrev_b32",
        "v_lshrrev_b32",
        "v_ashrrev_i32",
        "v_min_u32",
        "v_max_u32",
        "v_min_i32",
        "v_max_i32",
        "v_mul_u32_u24",
        "v_cvt_pkrtz_f16_f32",
    ] {
        // v5 starts as the instance number, so that the operations that
        // add to their result have something to add to.
        let code = Code::new()
            .i(asm::vop1("v_mov_b32", 5, O::V(3)))
            .i(asm::vop2(name, 5, O::V(0), 3))
            .i(asm::vop2(name, 6, O::Lit(0x4049_0FDB), 3))
            .i(asm::vop2(name, 7, O::S(8), 0))
            .i(export_position([5, 6, 7, 0]))
            .end();
        let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
        assert_eq!(outcome.exports.len(), 64, "{name}");
    }
}

#[test]
fn every_one_source_vector_operation_agrees_with_the_machine() {
    for name in [
        "v_mov_b32",
        "v_not_b32",
        "v_cvt_f32_i32",
        "v_cvt_f32_u32",
        "v_cvt_i32_f32",
        "v_cvt_u32_f32",
        "v_cvt_f16_f32",
        "v_cvt_f32_f16",
        "v_cvt_f32_ubyte0",
        "v_cvt_f32_ubyte1",
        "v_cvt_f32_ubyte2",
        "v_cvt_f32_ubyte3",
        "v_rcp_f32",
        "v_rcp_iflag_f32",
        "v_rsq_f32",
        "v_sqrt_f32",
        "v_floor_f32",
        "v_ceil_f32",
        "v_trunc_f32",
        "v_rndne_f32",
        "v_fract_f32",
        "v_sin_f32",
        "v_cos_f32",
        "v_exp_f32",
        "v_log_f32",
        "v_ffbl_b32",
        "v_ffbh_u32",
        "v_bfrev_b32",
    ] {
        let code = Code::new()
            .i(asm::vop1(name, 5, O::V(0)))
            .i(asm::vop1(name, 6, O::V(3)))
            .i(asm::vop1(name, 7, O::S(9)))
            .i(export_position([5, 6, 7, 0]))
            .end();
        run_both(&vertex(code), &vertex_data(), 64);
    }
}

#[test]
fn every_three_source_vector_operation_agrees_with_the_machine() {
    for name in [
        "v_fma_f32",
        "v_mad_f32",
        "v_mad_legacy_f32",
        "v_med3_f32",
        "v_min3_f32",
        "v_max3_f32",
        "v_bfe_u32",
        "v_bfe_i32",
        "v_bfi_b32",
        "v_mad_u32_u24",
        "v_lshl_add_u32",
        "v_add_lshl_u32",
        "v_add3_u32",
        "v_lshl_or_b32",
        "v_and_or_b32",
        "v_or3_b32",
        "v_xad_u32",
        "v_perm_b32",
        "v_med3_i32",
        "v_med3_u32",
        "v_min3_u32",
        "v_max3_u32",
        "v_mul_lo_u32",
        "v_mul_hi_u32",
        "v_pack_b32_f16",
        "v_bcnt_u32_b32",
    ] {
        // A third word per lane: the two inputs mixed.
        let code = Code::new()
            .i(asm::vop2("v_xor_b32", 1, O::V(0), 3))
            .i(asm::vop3(name, 5, [O::V(0), O::V(3), O::V(1)], 0, 0))
            .i(asm::vop3(name, 6, [O::V(3), O::S(8), O::V(0)], 0, 0))
            .i(asm::vop3(
                name,
                7,
                [O::V(1), O::V(0), O::Lit(0x0102_0C09)],
                0,
                0,
            ))
            .i(export_position([5, 6, 7, 1]))
            .end();
        run_both(&vertex(code), &vertex_data(), 64);
    }
}

#[test]
fn the_absolute_value_and_negation_of_float_sources_are_applied() {
    let code = Code::new()
        // |v0| - (-v3), and a fused multiply-add of -v0, |v3| and v0.
        .i(asm::vop3(
            "v_sub_f32",
            5,
            [O::V(0), O::V(3), O::K(0)],
            0b01,
            0b10,
        ))
        .i(asm::vop3(
            "v_fma_f32",
            6,
            [O::V(0), O::V(3), O::V(0)],
            0b010,
            0b001,
        ))
        .i(export_position([5, 6, 0, 3]))
        .end();
    let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    // Lane 1 holds tidy floats.
    let [sum, _, a, b] = position(&outcome, 1).map(f32::from_bits);
    assert_eq!(sum, a.abs() + b);
}

#[test]
fn every_vector_comparison_agrees_with_the_machine() {
    let float = [
        "lt", "eq", "le", "gt", "lg", "ge", "o", "u", "nge", "nlg", "ngt", "nle", "neq", "nlt",
        "f", "tru",
    ];
    let integer = ["lt", "eq", "le", "gt", "ne", "ge", "f", "t"];
    let names = float
        .iter()
        .map(|relation| format!("v_cmp_{relation}_f32"))
        .chain(
            integer
                .iter()
                .map(|relation| format!("v_cmp_{relation}_i32")),
        )
        .chain(
            integer
                .iter()
                .map(|relation| format!("v_cmp_{relation}_u32")),
        );
    for name in names {
        // The comparison picks which of two registers each lane keeps.
        let code = Code::new()
            .i(asm::vopc(&name, O::V(0), 3))
            .i(asm::vop2("v_cndmask_b32", 5, O::V(0), 3))
            .i(asm::vopc(&name, O::V(3), 3))
            .i(asm::vop2("v_cndmask_b32", 6, O::K(0), 3))
            .i(export_position([5, 6, 0, 3]))
            .end();
        run_both(&vertex(code), &vertex_data(), 64);
    }
}

#[test]
fn a_lane_outside_the_mask_keeps_its_old_value() {
    // if (v0 > v3) v5 = v0 + v3; else v5 = v0 - v3; then both go on.
    let code = Code::new()
        .i(asm::vop1("v_mov_b32", 5, O::Lit(0xDEAD_0000)))
        .i(asm::vopc("v_cmp_lt_u32", O::V(3), 0))
        .i(asm::sop1("s_and_saveexec_b64", O::S(10), O::Vcc))
        .branch("s_cbranch_execz", "else")
        .i(asm::vop2("v_add_nc_u32", 5, O::V(0), 3))
        .i(asm::vop1("v_mov_b32", 6, O::K(1)))
        .label("else")
        .i(asm::sop2("s_andn2_b64", O::Exec, O::S(10), O::Exec))
        .branch("s_cbranch_execz", "end")
        .i(asm::vop2("v_sub_nc_u32", 5, O::V(0), 3))
        .i(asm::vop1("v_mov_b32", 7, O::K(2)))
        .label("end")
        .i(asm::sop1("s_mov_b64", O::Exec, O::S(10)))
        .i(export_position([5, 6, 7, 0]))
        .end();
    let (function, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    assert!(!function.has_cross_lane());
    for lane in 0..64 {
        let (a, b) = (lane_word(lane, 1), lane_word(lane, 2));
        let expected = if a > b {
            [a.wrapping_add(b), 1, 0, a]
        } else {
            [a.wrapping_sub(b), 0, 2, a]
        };
        assert_eq!(position(&outcome, lane), expected, "lane {lane}");
    }
}

#[test]
fn a_loop_runs_each_lane_as_often_as_that_lane_needs() {
    // v6 = v0 & 7; do { v5 += v3; } while (v6-- > 0);
    let code = Code::new()
        .i(asm::vop2("v_and_b32", 6, O::K(7), 0))
        .i(asm::vop1("v_mov_b32", 5, O::K(0)))
        .i(asm::sop1("s_mov_b64", O::S(10), O::Exec))
        .label("loop")
        .i(asm::vop2("v_add_nc_u32", 5, O::V(5), 3))
        .i(asm::vopc("v_cmp_lt_u32", O::K(0), 6))
        .i(asm::vop2("v_add_nc_u32", 6, O::K(-1), 6))
        .i(asm::sop2("s_and_b64", O::Exec, O::Exec, O::Vcc))
        .branch("s_cbranch_execnz", "loop")
        .i(asm::sop1("s_mov_b64", O::Exec, O::S(10)))
        .i(export_position([5, 6, 0, 3]))
        .end();
    let (function, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    assert_eq!(ControlFlow::of(&function).loops.len(), 1);
    for lane in 0..64 {
        let trips = (lane_word(lane, 1) & 7) + 1;
        let step = lane_word(lane, 2);
        assert_eq!(position(&outcome, lane)[0], step.wrapping_mul(trips));
    }
}

/// A program that parks the numbers of `s8` and `s9` in lanes 3 and 40 of
/// `v20`, changes the registers, and takes the numbers out again into the
/// position; `also` comes before the export.
fn parking(also: Vec<u32>) -> Vec<u32> {
    let write = |register: u8, lane: i8| {
        asm::vop3(
            "v_writelane_b32",
            20,
            [O::S(register), O::K(lane), O::K(0)],
            0,
            0,
        )
    };
    let read = |register: u8, lane: i8| {
        asm::vop3(
            "v_readlane_b32",
            u32::from(register),
            [O::V(20), O::K(lane), O::K(0)],
            0,
            0,
        )
    };
    Code::new()
        .i(write(8, 3))
        .i(write(9, 40))
        .i(asm::sop2("s_add_u32", O::S(8), O::S(8), O::K(5)))
        .i(asm::sop2("s_add_u32", O::S(9), O::S(9), O::K(7)))
        .i(read(12, 3))
        .i(read(13, 40))
        // A lane nothing was parked in reads zero.
        .i(read(14, 9))
        .i(asm::vop1("v_mov_b32", 5, O::S(12)))
        .i(asm::vop1("v_mov_b32", 6, O::S(13)))
        .i(asm::vop1("v_mov_b32", 7, O::S(14)))
        .i(asm::vop1("v_mov_b32", 8, O::S(8)))
        .i(also)
        .i(export_position([5, 6, 7, 8]))
        .end()
}

#[test]
fn scalars_parked_in_lanes_of_a_vector_register_become_variables() {
    let (function, outcome) = run_both(&vertex(parking(Vec::new())), &vertex_data(), 64);
    // Nothing of it looks across lanes any more.
    assert!(!function.has_cross_lane(), "{function}");
    let lane_bits = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter(|inst| matches!(inst.op, Op::LaneBit { .. } | Op::ReadLane { .. }))
        .count();
    assert_eq!(lane_bits, 0, "{function}");
    for lane in [0, 3, 40, 63] {
        assert_eq!(
            position(&outcome, lane),
            [
                TestHost::new().user[8],
                TestHost::new().user[9],
                0,
                TestHost::new().user[8].wrapping_add(5)
            ]
        );
    }
}

#[test]
fn a_vector_register_that_is_also_read_as_a_vector_is_not_taken_for_parking() {
    // The register goes to the position as it is: lane 3 and lane 40 hold
    // what was written there and the others what they held before.
    let also = asm::vop1("v_mov_b32", 7, O::V(20));
    let (function, outcome) = run_both(&vertex(parking(also)), &vertex_data(), 64);
    assert!(function.has_cross_lane());
    assert_eq!(position(&outcome, 3)[2], TestHost::new().user[8]);
    assert_eq!(position(&outcome, 40)[2], TestHost::new().user[9]);
    assert_eq!(position(&outcome, 5)[2], 0);
}

#[test]
fn a_loop_over_the_first_lanes_value_serves_every_lane() {
    // The shape compilers give a resource index that differs per lane:
    // take the first live lane's index, serve the lanes that have the same
    // one, and go round with the rest.
    let code = Code::new()
        .i(asm::vop2("v_and_b32", 6, O::K(3), 0))
        .i(asm::sop1("s_mov_b64", O::S(10), O::Exec))
        .label("loop")
        .i(asm::vop1_to_scalar("v_readfirstlane_b32", 12, O::V(6)))
        .i(asm::vopc("v_cmp_eq_u32", O::S(12), 6))
        .i(asm::sop1("s_and_saveexec_b64", O::S(14), O::Vcc))
        .i(asm::sop2("s_lshl_b32", O::S(13), O::S(12), O::K(4)))
        .i(asm::vop2("v_add_nc_u32", 5, O::S(13), 3))
        .i(asm::sop2("s_xor_b64", O::Exec, O::Exec, O::S(14)))
        .branch("s_cbranch_execnz", "loop")
        .i(asm::sop1("s_mov_b64", O::Exec, O::S(10)))
        .i(export_position([5, 6, 0, 3]))
        .end();
    let (function, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    assert!(function.has_cross_lane());
    for lane in 0..64 {
        let index = lane_word(lane, 1) & 3;
        assert_eq!(
            position(&outcome, lane)[0],
            (index << 4).wrapping_add(lane_word(lane, 2))
        );
    }
    // A wave with fewer lanes behaves the same.
    run_both(
        &vertex(Code::new().i(export_position([0, 0, 0, 0])).end()),
        &vertex_data(),
        5,
    );
}

#[test]
fn a_branch_on_a_number_goes_the_same_way_in_every_lane() {
    for (user, expected) in [(7u32, 1u32), (8, 2)] {
        let code = Code::new()
            .i(asm::sopc("s_cmp_eq_u32", O::S(8), O::K(7)))
            .branch("s_cbranch_scc1", "seven")
            .i(asm::vop1("v_mov_b32", 5, O::K(2)))
            .branch("s_branch", "end")
            .label("seven")
            .i(asm::vop1("v_mov_b32", 5, O::K(1)))
            .label("end")
            // The condition also picks between two numbers and two masks.
            .i(asm::sopc("s_cmp_lt_u32", O::S(8), O::K(8)))
            .i(asm::sop2("s_cselect_b32", O::S(12), O::K(40), O::K(50)))
            .i(asm::sop2("s_cselect_b64", O::Vcc, O::Exec, O::K(0)))
            .i(asm::vop2("v_cndmask_b32", 6, O::K(3), 0))
            .i(asm::vop1("v_mov_b32", 7, O::S(12)))
            .i(export_position([5, 6, 7, 0]))
            .end();
        let program = vertex(code);
        let (_, outcome) = run_both_with(&program, &vertex_data(), 64, |host| host.user[8] = user);
        for lane in 0..64 {
            let words = position(&outcome, lane);
            assert_eq!(words[0], expected);
            assert_eq!(words[1], if user < 8 { lane_word(lane, 1) } else { 3 });
            assert_eq!(words[2], if user < 8 { 40 } else { 50 });
        }
    }
}

#[test]
fn scalar_arithmetic_and_its_condition_codes_agree_with_the_machine() {
    // Each operation's result and the condition code after it are folded
    // into a running sum that every lane exports.
    let mut code = Code::new().i(asm::sop1("s_mov_b32", O::S(20), O::K(0)));
    let fold = |code: Code| {
        code.i(asm::sop2("s_addc_u32", O::S(20), O::S(20), O::S(12)))
            .i(asm::sop2("s_lshl_b32", O::S(21), O::S(20), O::K(3)))
            .i(asm::sop2("s_xor_b32", O::S(20), O::S(20), O::S(21)))
    };
    for name in [
        "s_add_u32",
        "s_add_i32",
        "s_sub_u32",
        "s_sub_i32",
        "s_addc_u32",
        "s_subb_u32",
        "s_min_i32",
        "s_min_u32",
        "s_max_i32",
        "s_max_u32",
        "s_mul_i32",
        "s_mul_hi_u32",
        "s_lshl_b32",
        "s_lshr_b32",
        "s_ashr_i32",
        "s_bfe_u32",
        "s_bfe_i32",
        "s_and_b32",
        "s_or_b32",
        "s_xor_b32",
        "s_andn2_b32",
        "s_orn2_b32",
        "s_nand_b32",
        "s_nor_b32",
        "s_xnor_b32",
    ] {
        code = fold(code.i(asm::sop2(name, O::S(12), O::S(8), O::S(9))));
        code = fold(code.i(asm::sop2(name, O::S(12), O::S(10), O::Lit(0x0005_0003))));
    }
    for name in [
        "s_brev_b32",
        "s_not_b32",
        "s_bcnt1_i32_b32",
        "s_ff1_i32_b32",
    ] {
        code = fold(code.i(asm::sop1(name, O::S(12), O::S(9))));
    }
    for name in [
        "s_cmp_eq_i32",
        "s_cmp_lg_i32",
        "s_cmp_gt_i32",
        "s_cmp_ge_i32",
        "s_cmp_lt_i32",
        "s_cmp_le_i32",
        "s_cmp_eq_u32",
        "s_cmp_lg_u32",
        "s_cmp_gt_u32",
        "s_cmp_ge_u32",
        "s_cmp_lt_u32",
        "s_cmp_le_u32",
    ] {
        code = fold(code.i(asm::sopc(name, O::S(8), O::S(9))));
        code = fold(code.i(asm::sopc(name, O::S(11), O::S(11))));
    }
    code = fold(code.i(asm::sopk("s_movk_i32", O::S(12), 0xFFF0)));
    code = fold(code.i(asm::sopk("s_addk_i32", O::S(12), 0x7FFF)));
    code = fold(code.i(asm::sopk("s_mulk_i32", O::S(12), 0xFFFD)));
    code = fold(code.i(asm::sopk("s_cmpk_lt_i32", O::S(8), 0x8000)));
    code = fold(code.i(asm::sopk("s_cmpk_gt_u32", O::S(8), 0x8000)));
    code = fold(code.i(asm::sop2("s_lshl_b64", O::S(12), O::S(8), O::S(10))));
    code = fold(code.i(asm::sop2("s_lshr_b64", O::S(12), O::S(8), O::K(37))));
    code = fold(code.i(asm::sop2("s_ashr_i64", O::S(12), O::S(8), O::S(11))));
    let code = code
        .i(asm::vop1("v_mov_b32", 5, O::S(20)))
        .i(export_position([5, 5, 5, 5]))
        .end();
    for seed in 0..6u64 {
        run_both_with(&vertex(code.clone()), &vertex_data(), 4, |host| {
            for register in 8..12 {
                host.user[register] = match seed {
                    0 => mix(seed * 64 + register as u64) as u32,
                    1 => 0,
                    2 => u32::MAX,
                    3 => 0x8000_0000 >> (register - 8),
                    4 => (register - 8) as u32,
                    _ => mix(seed + register as u64) as u32 & 0x3F,
                };
            }
        });
    }
}

#[test]
fn counts_of_lanes_are_explicit_cross_lane_operations() {
    // Kill some lanes, then count the live ones, find the first, and give
    // every lane its number among them.
    let code = Code::new()
        .i(asm::sop1("s_mov_b64", O::S(10), O::Exec))
        .i(asm::vopc("v_cmpx_lt_u32", O::V(3), 0))
        .i(asm::sop1("s_bcnt1_i32_b64", O::S(12), O::Exec))
        .i(asm::sop1("s_ff1_i32_b64", O::S(13), O::Exec))
        .i(asm::vop3(
            "v_mbcnt_lo_u32_b32",
            6,
            [O::Exec, O::K(0), O::K(0)],
            0,
            0,
        ))
        .i(asm::vop3(
            "v_mbcnt_hi_u32_b32",
            6,
            [O::ExecHigh, O::V(6), O::K(0)],
            0,
            0,
        ))
        .i(asm::vop1("v_mov_b32", 5, O::S(12)))
        .i(asm::vop1("v_mov_b32", 7, O::S(13)))
        .i(asm::sop1("s_mov_b64", O::Exec, O::S(10)))
        .i(export_position([5, 6, 7, 0]))
        .end();
    let (function, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    let counted = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter(|inst| {
            matches!(
                inst.op,
                Op::CountLanes(_) | Op::FirstLaneIndex(_) | Op::LanesBelow { .. }
            )
        })
        .count();
    assert_eq!(counted, 4);
    let live: Vec<usize> = (0..64)
        .filter(|&lane| lane_word(lane, 1) > lane_word(lane, 2))
        .collect();
    for (rank, &lane) in live.iter().enumerate() {
        assert_eq!(
            position(&outcome, lane)[..3],
            [live.len() as u32, rank as u32, live[0] as u32]
        );
    }
}

#[test]
fn quad_reads_reach_the_neighbouring_lanes() {
    let code = Code::new()
        // Lane 1 of each quad everywhere (the horizontal neighbour for
        // lanes 0 and 2), and the quad mirrored.
        .i(asm::swizzle(5, 0, 0x8055))
        .i(asm::mov_quad(6, 0, 0xB1, true, true))
        // The same with some lanes dead: a dead source gives zero to a
        // swizzle and is read all the same with the fetch-inactive bit.
        .i(asm::sop1("s_mov_b64", O::S(10), O::Exec))
        .i(asm::vopc("v_cmpx_lt_u32", O::V(3), 0))
        .i(asm::swizzle(7, 0, 0x80AA))
        .i(asm::mov_quad(8, 0, 0x4E, true, true))
        .i(asm::mov_quad(9, 0, 0x1B, false, true))
        .i(asm::sop1("s_mov_b64", O::Exec, O::S(10)))
        .i(export_position([5, 6, 7, 8]))
        .i(asm::exp(32, [9, 0, 0, 0], false))
        .end();
    let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    let own = |lane: usize| lane_word(lane, 1);
    for lane in 0..64 {
        let quad = lane & !3;
        let words = position(&outcome, lane);
        assert_eq!(words[0], own(quad + 1));
        assert_eq!(words[1], own(quad + [1, 0, 3, 2][lane & 3]));
    }
}

#[test]
fn sub_register_forms_read_and_write_parts_of_registers() {
    let code = Code::new()
        // Byte 1 of v0 plus the high half of v3; the low half of v0,
        // sign-extended, minus byte 3 of v3; a sum written to byte 2 of a
        // register whose other bytes are kept; and a byte written to byte 0
        // with its top bit repeated above.
        .i(asm::vop2_sub(
            "v_add_nc_u32",
            5,
            0,
            3,
            6,
            0,
            [1, 5],
            [false, false],
        ))
        .i(asm::vop2_sub(
            "v_sub_nc_u32",
            6,
            0,
            3,
            6,
            0,
            [4, 3],
            [true, false],
        ))
        .i(asm::vop1("v_mov_b32", 7, O::V(0)))
        .i(asm::vop2_sub(
            "v_add_nc_u32",
            7,
            0,
            3,
            2,
            2,
            [6, 6],
            [false, false],
        ))
        .i(asm::vop2_sub(
            "v_or_b32",
            8,
            0,
            3,
            0,
            1,
            [0, 0],
            [false, false],
        ))
        .i(export_position([5, 6, 7, 8]))
        .end();
    let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    for lane in 0..64 {
        let (a, b) = (lane_word(lane, 1), lane_word(lane, 2));
        let words = position(&outcome, lane);
        assert_eq!(words[0], (a >> 8 & 0xFF) + (b >> 16));
        assert_eq!(words[1], (a as i16 as i32 as u32).wrapping_sub(b >> 24));
        let sum = a.wrapping_add(b) & 0xFF;
        assert_eq!(words[2], a & 0xFF00_FFFF | sum << 16);
        assert_eq!(words[3], (a | b) as u8 as i8 as i32 as u32);
    }
}

#[test]
fn carries_travel_through_lane_masks() {
    // A 64-bit addition per lane: the low halves with a carry out, the
    // high halves with the carry in.
    let code = Code::new()
        .i(asm::vop3_carry(
            "v_add_co_u32",
            5,
            O::Vcc,
            [O::V(0), O::V(3), O::K(0)],
        ))
        .i(asm::vop2("v_add_co_ci_u32", 6, O::K(1), 3))
        .i(asm::vop3_carry(
            "v_sub_co_u32",
            7,
            O::S(12),
            [O::V(0), O::V(3), O::K(0)],
        ))
        .i(asm::vop2("v_cndmask_b32", 8, O::K(0), 0))
        .i(export_position([5, 6, 7, 8]))
        .end();
    let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    for lane in 0..64 {
        let (a, b) = (lane_word(lane, 1), lane_word(lane, 2));
        let wide = u64::from(a) + u64::from(b);
        let words = position(&outcome, lane);
        assert_eq!(words[0], wide as u32);
        assert_eq!(
            words[1],
            1u32.wrapping_add(b).wrapping_add((wide >> 32) as u32)
        );
        assert_eq!(words[2], a.wrapping_sub(b));
    }
}

#[test]
fn memory_is_read_by_address_and_through_descriptors() {
    // s8 holds a table's 32-bit address; the code zeroes the high half
    // itself. The table holds a buffer descriptor, through which a
    // constant and then per-lane words are read. s10 and s11 hold a root
    // view's address, around which the code builds a descriptor.
    let code = Code::new()
        .i(asm::sop1("s_mov_b32", O::S(9), O::K(0)))
        .i(asm::smem("s_load_dwordx4", 12, 8, 0x20, None))
        .i(asm::smem("s_buffer_load_dword", 16, 12, 8, None))
        .i(asm::smem("s_buffer_load_dwordx2", 18, 12, 0x7FC, None))
        .i(asm::vop2("v_and_b32", 6, O::K(31), 0))
        .i(asm::mubuf("buffer_load_dword", 5, 6, 12, 4, true, false))
        .i(asm::sop1("s_mov_b32", O::S(20), O::S(10)))
        .i(asm::sop2(
            "s_or_b32",
            O::S(21),
            O::S(11),
            O::Lit(0x0001_0000),
        ))
        .i(asm::sop1("s_mov_b32", O::S(22), O::K(-1)))
        .i(asm::sop1("s_mov_b32", O::S(23), O::Lit(0x1101_4FAC)))
        .i(asm::vop2("v_lshlrev_b32", 7, O::K(2), 6))
        .i(asm::mubuf("buffer_load_dwordx2", 8, 7, 20, 0, false, true))
        .i(asm::global("global_load_dword", 10, 7, 0, 10, 8))
        .i(asm::vop2("v_add_nc_u32", 11, O::S(16), 5))
        .i(asm::vop2("v_add_nc_u32", 11, O::S(18), 11))
        .i(asm::vop2("v_add_nc_u32", 11, O::S(19), 11))
        .i(asm::mubuf(
            "buffer_store_dword",
            11,
            7,
            20,
            0x400,
            false,
            true,
        ))
        .i(export_position([5, 8, 9, 10]))
        .end();
    let table = 0x1_0000u64;
    let buffer = 0x12_3456_7000u64;
    let view = 0x34_0000_0100u64;
    let prepare = |host: &mut TestHost| {
        host.user[8] = table as u32;
        host.user[10] = view as u32;
        host.user[11] = (view >> 32) as u32;
        // A descriptor of 32 records of 16 bytes, bound by index and
        // offset.
        host.write_words(
            table + 0x20,
            &[buffer as u32, (buffer >> 32) as u32 | 16 << 16, 32, 0],
        );
    };
    let (function, outcome) = run_both_with(&vertex(code), &vertex_data(), 64, prepare);
    assert!(!function.has_cross_lane());
    let mut host = TestHost::new();
    prepare(&mut host);
    for lane in 0..64 {
        let index = u64::from(lane_word(lane, 1) & 31);
        let words = position(&outcome, lane);
        assert_eq!(words[0], host.load(buffer + 16 * index + 4, 32));
        assert_eq!(words[1], host.load(view + 4 * index, 32));
        assert_eq!(words[2], host.load(view + 4 * index + 4, 32));
        assert_eq!(words[3], host.load(view + 4 * index + 8, 32));
    }
    // The scalar load at offset 8 is inside the first record; the one at
    // 0x7FC is past the record's 16 bytes and reads zero. Each lane stores
    // the constant plus its own word.
    let constant = host.load(buffer + 8, 32);
    for lane in 0..64 {
        let index = u64::from(lane_word(lane, 1) & 31);
        let at = view + 0x400 + 4 * index;
        let bytes: Vec<u8> = (0..4).map(|byte| outcome.stores[&(at + byte)]).collect();
        assert_eq!(
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            constant.wrapping_add(host.load(buffer + 16 * index + 4, 32))
        );
    }
}

#[test]
fn typed_buffer_loads_convert_by_their_format() {
    // Vertices of 20 bytes fetched by index: a position of three floats at
    // 0, a colour of four normalised bytes at 12.
    let code = Code::new()
        .i(asm::sop1("s_mov_b32", O::S(20), O::S(10)))
        .i(asm::sop2("s_or_b32", O::S(21), O::S(11), O::Lit(20 << 16)))
        .i(asm::sop1("s_mov_b32", O::S(22), O::K(40)))
        .i(asm::sop1("s_mov_b32", O::S(23), O::Lit(0x1000_0000)))
        .i(asm::vop2("v_and_b32", 6, O::K(63), 0))
        .i(asm::mtbuf("tbuffer_load_format_xyz", 74, 8, 6, 20, 0))
        .i(asm::mtbuf("tbuffer_load_format_xyzw", 56, 12, 6, 20, 12))
        .i(export_position([8, 9, 10, 12]))
        .i(asm::exp(32, [12, 13, 14, 15], false))
        .end();
    let view = 0x20_0000_0000u64;
    let prepare = |host: &mut TestHost| {
        host.user[10] = view as u32;
        host.user[11] = (view >> 32) as u32;
    };
    let (_, outcome) = run_both_with(&vertex(code), &vertex_data(), 64, prepare);
    let mut host = TestHost::new();
    prepare(&mut host);
    for lane in 0..64 {
        let index = u64::from(lane_word(lane, 1) & 63);
        let words = position(&outcome, lane);
        let (target, colour) = outcome.exports[&lane][1];
        assert_eq!(target, ExportTarget::Parameter(0));
        if index >= 40 {
            // Past the last record: zeroes.
            assert_eq!(words[..3], [0, 0, 0]);
            assert_eq!(colour, [Some(0); 4]);
            continue;
        }
        let at = view + 20 * index;
        assert_eq!(words[0], host.load(at, 32));
        assert_eq!(words[2], host.load(at + 8, 32));
        let red = host.load(at + 12, 8) as f32 / 255.0;
        assert_eq!(colour[0], Some(red.to_bits()));
        let alpha = host.load(at + 15, 8) as f32 / 255.0;
        assert_eq!(colour[3], Some(alpha.to_bits()));
    }
}

#[test]
fn a_sampler_built_from_constants_is_passed_on_as_its_words() {
    // An image descriptor loaded from a table, a sampler from the heap,
    // and a sampler the code builds from constants.
    let code = Code::new()
        .i(asm::sop1("s_mov_b32", O::S(9), O::K(0)))
        .i(asm::smem("s_load_dwordx8", 12, 8, 0x40, None))
        .i(asm::smem("s_load_dwordx4", 20, 8, 0x80, None))
        .i(asm::sopk("s_movk_i32", O::S(24), 0x400))
        .i(asm::sop1("s_mov_b32", O::S(25), O::Lit(0x00C0_0000)))
        .i(asm::sop1("s_mov_b32", O::S(26), O::Lit(0x18F0_0000)))
        .i(asm::sop1("s_mov_b32", O::S(27), O::K(0)))
        .i(asm::vop1("v_mov_b32", 4, O::V(0)))
        .i(asm::vop1("v_mov_b32", 5, O::V(3)))
        .i(asm::mimg("image_sample", 0xF, 1, 8, 4, 12, 20))
        .i(asm::mimg("image_sample_lz", 0x5, 1, 12, 4, 12, 24))
        .i(asm::mimg("image_load", 0xF, 1, 16, 4, 12, 0))
        .i(asm::mimg("image_get_lod", 0x3, 1, 20, 4, 12, 20))
        .i(export_position([8, 9, 10, 11]))
        .i(asm::exp(32, [12, 13, 16, 19], false))
        .i(asm::exp(33, [20, 21, 0, 0], false))
        .end();
    let prepare = |host: &mut TestHost| {
        host.user[8] = 0x2_0000;
        host.write_words(0x2_0040, &[1, 2, 3, 4, 5, 77, 2, 8]);
        host.write_words(0x2_0080, &[0, 0, 0, 0x8000_0123]);
    };
    let (function, _) = run_both_with(&vertex(code), &vertex_data(), 64, prepare);
    let images: Vec<&ImageOp> = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter_map(|inst| match &inst.op {
            Op::Image(image) => Some(&**image),
            _ => None,
        })
        .collect();
    assert_eq!(images.len(), 4);
    assert_eq!(images[3].kind, ImageKind::QueryLevel);
    assert!(matches!(images[0].sampler, Some(Sampler::Heap(_))));
    assert_eq!(images[0].level, Level::Implicit);
    assert_eq!(
        images[1].sampler,
        Some(Sampler::Immediate([0x400, 0x00C0_0000, 0x18F0_0000, 0]))
    );
    assert_eq!(images[1].level, Level::Zero);
    assert_eq!(images[2].sampler, None);
    assert_eq!(images[2].kind, ImageKind::Load);
    assert_eq!(images[2].coordinates.len(), 2);
}

#[test]
fn sixteen_bit_image_addresses_are_unpacked_two_to_a_register() {
    let code = Code::new()
        .i(asm::sop1("s_mov_b32", O::S(9), O::K(0)))
        .i(asm::smem("s_load_dwordx8", 12, 8, 0x40, None))
        .i(asm::smem("s_load_dwordx4", 20, 8, 0x80, None))
        .i(asm::vop1("v_mov_b32", 4, O::V(0)))
        .i(asm::vop1("v_mov_b32", 5, O::V(3)))
        // A load of a 2D array level: x and y in one register, the slice
        // and the mip in the next.
        .i(asm::with_short_addresses(asm::mimg(
            "image_load_mip",
            0xF,
            5,
            8,
            4,
            12,
            0,
        )))
        // A sample at a level: s and t as half floats, the level after.
        .i(asm::with_short_addresses(asm::mimg(
            "image_sample_l",
            0xF,
            1,
            12,
            4,
            12,
            20,
        )))
        // A sample with a bias, whose register holds a half float in its
        // low half, before the packed coordinates.
        .i(asm::with_short_addresses(asm::mimg(
            "image_sample_b",
            0x3,
            1,
            16,
            4,
            12,
            20,
        )))
        .i(export_position([8, 9, 12, 16]))
        .end();
    let prepare = |host: &mut TestHost| {
        host.user[8] = 0x2_0000;
        host.write_words(0x2_0040, &[1, 2, 3, 4, 5, 77, 1, 8]);
    };
    let (function, _) = run_both_with(&vertex(code), &vertex_data(), 64, prepare);
    let images: Vec<&ImageOp> = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter_map(|inst| match &inst.op {
            Op::Image(image) => Some(&**image),
            _ => None,
        })
        .collect();
    assert_eq!(images[0].coordinates.len(), 3);
    assert!(images[0].mip.is_some());
    assert_eq!(images[1].coordinates.len(), 2);
    assert!(matches!(images[1].level, Level::Explicit(_)));
    assert!(images[2].bias.is_some());
    assert_eq!(images[2].coordinates.len(), 2);
}

/// A pixel program over a 64-lane wave.
fn pixel(code: Vec<u32>) -> Program {
    program(HardwareStage::Pixel, WaveSize::Wave64, code, &[8])
}

#[test]
fn a_lane_that_is_not_live_at_the_final_export_is_discarded() {
    // discard if (weight i <= weight j); the colour is the two weights.
    let code = Code::new()
        .i(asm::vopc("v_cmpx_lt_f32", O::V(1), 0))
        .i(asm::exp(0, [0, 1, 0, 1], true))
        .end();
    let (function, outcome) = run_both(&pixel(code), &pixel_data(), 64);
    assert!(
        function
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .any(|inst| matches!(inst.op, Op::Discard(_)))
    );
    let mut host = TestHost::new();
    let weight = |host: &mut TestHost, lane, second| {
        f32::from_bits(host.input(
            lane,
            Input::Weight {
                weights: crate::ir::Weights::PerspectiveCenter,
                second,
            },
        ))
    };
    for lane in 0..64 {
        let kept = weight(&mut host, lane, true) < weight(&mut host, lane, false);
        assert_eq!(outcome.discards.contains(&lane), !kept, "lane {lane}");
        assert_eq!(outcome.exports.contains_key(&lane), kept);
    }
}

#[test]
fn a_lane_that_ends_without_its_final_export_is_discarded() {
    // The early out of a long program: lanes that are killed skip
    // everything, the export included.
    let code = Code::new()
        .i(asm::vopc("v_cmpx_lt_f32", O::V(1), 0))
        .branch("s_cbranch_execz", "end")
        .i(asm::vop2("v_add_f32", 2, O::V(0), 1))
        .i(asm::exp(0, [2, 2, 2, 2], true))
        .label("end")
        .end();
    let (_, outcome) = run_both(&pixel(code), &pixel_data(), 64);
    assert_eq!(outcome.discards.len() + outcome.exports.len(), 64);
    assert!(!outcome.discards.is_empty() && !outcome.exports.is_empty());
    // With every lane killed, nothing is exported at all.
    let code = Code::new()
        .i(asm::vopc("v_cmpx_f_f32", O::V(1), 0))
        .branch("s_cbranch_execz", "end")
        .i(asm::exp(0, [2, 2, 2, 2], true))
        .label("end")
        .end();
    let (_, outcome) = run_both(&pixel(code), &pixel_data(), 64);
    assert_eq!(outcome.discards.len(), 64);
}

#[test]
fn interpolation_with_the_weights_as_given_reads_as_one_interpolated_input() {
    let code = Code::new()
        .i(asm::interp("v_interp_p1_f32", 2, 0, 1, 0))
        .i(asm::interp("v_interp_p1_f32", 3, 0, 1, 1))
        .i(asm::interp("v_interp_p2_f32", 2, 1, 1, 0))
        .i(asm::interp("v_interp_p2_f32", 3, 1, 1, 1))
        // A flat attribute, and a first half whose weight was changed,
        // which has to be evaluated as written.
        .i(asm::interp("v_interp_mov_f32", 4, 2, 0, 3))
        .i(asm::vop2("v_add_f32", 6, O::V(0), 1))
        .i(asm::interp("v_interp_p1_f32", 5, 6, 0, 0))
        .i(asm::interp("v_interp_p2_f32", 5, 1, 0, 0))
        .i(asm::exp(0, [2, 3, 4, 5], true))
        .end();
    let (function, outcome) = run_both(&pixel(code), &pixel_data(), 64);
    let inputs: Vec<Input> = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter_map(|inst| match inst.op {
            Op::Input(input) => Some(input),
            _ => None,
        })
        .collect();
    let interpolated = inputs
        .iter()
        .filter(|input| matches!(input, Input::Interpolated { attribute: 1, .. }))
        .count();
    assert_eq!(interpolated, 2);
    // Attribute 1 is never read vertex by vertex; attribute 0 is.
    assert!(
        !inputs
            .iter()
            .any(|input| matches!(input, Input::Attribute { attribute: 1, .. }))
    );
    assert!(inputs.contains(&Input::Attribute {
        attribute: 0,
        channel: 3,
        term: VertexTerm::P0
    }));
    assert_eq!(outcome.exports.len(), 64);
    assert_eq!(function.interface.pixel_inputs.len(), 2);
    assert_eq!(function.interface.pixel_inputs[1].row, 2);
}

#[test]
fn a_program_of_32_lanes_keeps_its_masks_in_single_registers() {
    let code = Code::new()
        .i(asm::vopc("v_cmp_lt_f32", O::V(1), 0))
        .i(asm::sop1("s_and_saveexec_b32", O::S(10), O::Vcc))
        .branch("s_cbranch_execz", "end")
        .i(asm::vop2("v_add_f32", 2, O::V(0), 1))
        .label("end")
        .i(asm::sop2("s_or_b32", O::Exec, O::Exec, O::S(10)))
        // A count of the lanes the comparison held in, as a number.
        .i(asm::sop1("s_bcnt1_i32_b32", O::S(12), O::Vcc))
        .i(asm::vop1("v_mov_b32", 3, O::S(12)))
        .i(asm::exp(0, [2, 3, 0, 1], true))
        .end();
    let program = program(HardwareStage::Pixel, WaveSize::Wave32, code, &[8]);
    let (function, outcome) = run_both(&program, &pixel_data(), 32);
    assert_eq!(function.wave, WaveSize::Wave32);
    assert_eq!(outcome.exports.len(), 32);
    assert!(outcome.discards.is_empty());
}

#[test]
fn conversions_to_half_floats_follow_the_rounding_mode_the_program_sets() {
    let code = Code::new()
        .i(asm::vop1("v_cvt_f16_f32", 5, O::V(0)))
        .i(asm::setreg(1, 2, 2, 3))
        .i(asm::vop1("v_cvt_f16_f32", 6, O::V(0)))
        .i(asm::setreg(1, 2, 2, 0))
        .i(asm::vop1("v_cvt_f16_f32", 7, O::V(0)))
        // The base of scratch memory is set and never used.
        .i(asm::setreg(20, 0, 32, 0))
        .i(asm::setreg(21, 0, 32, 0x400))
        .i(export_position([5, 6, 7, 0]))
        .end();
    let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    let differ = (0..64)
        .filter(|&lane| position(&outcome, lane)[0] != position(&outcome, lane)[1])
        .count();
    assert!(differ > 0);
    for lane in 0..64 {
        let words = position(&outcome, lane);
        assert_eq!(words[0], words[2]);
    }
    // A rounding mode of 32-bit floats other than to nearest is refused.
    let code = Code::new().i(asm::setreg(1, 0, 2, 1)).end();
    let error = lift(&vertex(code), &vertex_data()).unwrap_err();
    assert_eq!(error.instruction, "s_setreg_imm32_b32");
}

#[test]
fn a_vertex_programs_attributes_arrive_at_its_fetch_call() {
    let code = Code::new()
        .i(asm::sop1("s_swappc_b64", O::S(4), O::S(4)))
        .i(asm::vop1("v_mov_b32", 7, O::One))
        .i(export_position([4, 5, 6, 7]))
        .i(asm::exp(32, [8, 9, 10, 11], false))
        .end();
    let data = StageData::Vertex(VertexData {
        inputs: vec![
            VertexInput {
                usage: 6,
                row: 0,
                mask: 7,
                register: 4,
                highest_component: 2,
            },
            VertexInput {
                usage: 6,
                row: 1,
                mask: 15,
                register: 8,
                highest_component: 3,
            },
        ],
        outputs: Vec::new(),
    });
    let (function, outcome) = run_both(&vertex(code), &data, 64);
    assert!(!function.has_cross_lane());
    let mut host = TestHost::new();
    for lane in [0, 17, 63] {
        let attribute = |host: &mut TestHost, row, channel| {
            host.input(lane, Input::VertexAttribute { row, channel })
        };
        let words = position(&outcome, lane);
        assert_eq!(words[0], attribute(&mut host, 0, 0));
        assert_eq!(words[2], attribute(&mut host, 0, 2));
        assert_eq!(words[3], 1f32.to_bits());
        let (_, colour) = outcome.exports[&lane][1];
        assert_eq!(colour[3], Some(attribute(&mut host, 1, 3)));
    }
}

#[test]
fn a_far_jump_is_followed_and_a_table_after_the_code_is_read() {
    // s_getpc, add a distance, jump: the form compilers use when a branch
    // cannot reach. Then a table of constants read relative to the code.
    let mut words = Code::new()
        .i(asm::sop1("s_getpc_b64", O::S(12), O::S(0)))
        .i(asm::sop2("s_add_u32", O::S(12), O::S(12), O::Lit(20)))
        .i(asm::sop2("s_addc_u32", O::S(13), O::S(13), O::K(0)))
        .i(asm::sop1("s_setpc_b64", O::S(0), O::S(12)))
        // Skipped by the jump.
        .i(asm::vop1("v_mov_b32", 5, O::K(9)))
        // Lands here: 4 + 20 bytes after the first instruction's end.
        .i(asm::sop1("s_getpc_b64", O::S(14), O::S(0)))
        .i(asm::sop2("s_lshl_b32", O::S(16), O::S(8), O::K(2)))
        .i(asm::sop2("s_add_u32", O::S(14), O::S(14), O::S(16)))
        .i(asm::sop2("s_addc_u32", O::S(15), O::S(15), O::K(0)))
        .i(asm::smem("s_load_dword", 17, 14, 36, None))
        .i(asm::vop1("v_mov_b32", 6, O::S(17)))
        .i(export_position([5, 6, 0, 3]))
        .end();
    // The table sits right after s_endpgm.
    let table = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
    let table_at = 4 * words.len() as u32;
    words.extend(table);
    // The second s_getpc gives the offset just after itself; the load adds
    // 36, which must land on the table.
    let base = crate::gfx10::walk(&words, WaveSize::Wave64)
        .instructions
        .iter()
        .filter(|instruction| instruction.mnemonic == Some("s_getpc_b64"))
        .nth(1)
        .unwrap()
        .end();
    assert_eq!(base + 36, table_at);
    for index in 0..4u32 {
        let (function, outcome) =
            run_both_with(&vertex(words.clone()), &vertex_data(), 4, |host| {
                host.user[8] = index
            });
        assert_eq!(position(&outcome, 0)[..2], [0, table[index as usize]]);
        assert!(
            function
                .blocks
                .iter()
                .flat_map(|block| &block.insts)
                .any(|inst| matches!(inst.op, Op::LoadCode { .. }))
        );
    }
}

#[test]
fn a_compute_program_gets_its_thread_and_group_numbers() {
    // Four user-data registers, then the group's x and y; the thread's x
    // and y in v0 and v1.
    let code = Code::new()
        .i(asm::vop2("v_add_nc_u32", 5, O::S(8), 0))
        .i(asm::vop2("v_add_nc_u32", 6, O::S(9), 1))
        .i(asm::sop1("s_mov_b32", O::S(20), O::S(4)))
        .i(asm::sop2(
            "s_or_b32",
            O::S(21),
            O::S(5),
            O::Lit(0x0001_0000),
        ))
        .i(asm::sop1("s_mov_b32", O::S(22), O::K(-1)))
        .i(asm::sop1("s_mov_b32", O::S(23), O::Lit(0x1000_0000)))
        .i(asm::vop2("v_lshlrev_b32", 7, O::K(3), 0))
        .i(asm::mubuf("buffer_store_dwordx2", 5, 7, 20, 0, false, true))
        .end();
    let mut program = program(
        HardwareStage::Compute,
        WaveSize::Wave64,
        code,
        &[4, 5, 6, 7],
    );
    program.stage = HardwareStage::Compute;
    let data = StageData::Compute(ComputeData {
        // Eight user registers, group x and y, two thread components.
        resources: 8 << 1 | 0b011 << 7 | 1 << 11,
        group_size: [8, 8, 1],
    });
    let (function, outcome) = run_both(&program, &data, 64);
    assert_eq!(function.interface.group_size, [8, 8, 1]);
    assert_eq!(outcome.stores.len(), 64 * 8);

    // A program whose parameters do not fit the registers is refused.
    let mut spilled = program.clone();
    spilled.root_parameters[0].user_data = 16;
    let error = lift(&spilled, &data).unwrap_err();
    assert_eq!(error.offset, None);
    assert!(error.reason.contains("spilled"));
}

#[test]
fn a_pixel_programs_colour_exports_are_patched_before_lifting() {
    // As stored: the site holds a conversion to half floats and a packed
    // export. The four-raw-words variant holds four s_nop and the export
    // of the four registers themselves.
    let half = Code::new()
        .i(asm::sopp("s_nop", 0))
        .i(asm::sopp("s_nop", 0))
        .i(asm::vop2("v_cvt_pkrtz_f16_f32", 4, O::V(0), 1))
        .i(asm::vop2("v_cvt_pkrtz_f16_f32", 5, O::V(0), 1));
    let mut stored = Code::new().i(asm::vop2("v_add_f32", 2, O::V(0), 1)).end();
    stored.pop();
    let site = stored.len() as u32;
    let mut half_words = half.end();
    half_words.pop();
    // A packed export: two registers, compressed, done.
    half_words.extend([0x3E << 26 | 1 << 11 | 1 << 10 | 0xF, 0x0000_0504]);
    stored.extend(&half_words);
    stored.extend(asm::sopp("s_endpgm", 0));
    let mut raw = Vec::new();
    for _ in 0..4 {
        raw.extend(asm::sopp("s_nop", 0));
    }
    raw.extend(asm::exp(0, [2, 0, 1, 2], true));
    let mut variants = [[0u32; 10]; 10];
    variants[4][..6].copy_from_slice(&half_words);
    variants[9][..6].copy_from_slice(&raw);
    let StageData::Pixel(mut data) = pixel_data() else {
        unreachable!();
    };
    data.patches.push(ExportPatch {
        target: 0,
        code_word: site,
        variants,
    });
    let data = StageData::Pixel(data);
    let (function, outcome) = run_both(&pixel(stored.clone()), &data, 64);
    let exports: Vec<&Op> = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .map(|inst| &inst.op)
        .filter(|op| matches!(op, Op::Export { .. }))
        .collect();
    assert_eq!(exports.len(), 1);
    assert!(matches!(
        exports[0],
        Op::Export {
            target: ExportTarget::Color(0),
            ..
        }
    ));
    assert_eq!(outcome.exports.len(), 64);

    // Without its patch the stored program's packed export is refused.
    let error = lift(&pixel(stored), &pixel_data()).unwrap_err();
    assert_eq!(error.instruction, "exp");
}

#[test]
fn an_instruction_without_a_rule_fails_the_program_and_names_itself() {
    let code = Code::new()
        .i(asm::vop1("v_mov_b32", 5, O::V(0)))
        .i(asm::vop3("v_add_f64", 6, [O::V(0), O::V(2), O::K(0)], 0, 0))
        .i(export_position([5, 5, 5, 5]))
        .end();
    let error = lift(&vertex(code), &vertex_data()).unwrap_err();
    assert_eq!(error.instruction, "v_add_f64");
    assert_eq!(error.offset, Some(4));
    assert!(error.to_string().contains("v_add_f64 at 0x4"));

    // A mesh program is refused as a whole.
    let mesh = program(
        HardwareStage::Mesh,
        WaveSize::Wave64,
        Code::new().end(),
        &[],
    );
    let error = lift(&mesh, &StageData::NotRead).unwrap_err();
    assert_eq!(error.offset, None);
}

#[test]
fn a_loop_entered_in_its_middle_is_lifted_to_a_reducible_graph() {
    // A rotated loop whose first test jumps into the body.
    let code = Code::new()
        .i(asm::sop1("s_mov_b32", O::S(12), O::K(0)))
        .i(asm::sopc("s_cmp_eq_u32", O::S(8), O::K(0)))
        .branch("s_cbranch_scc1", "middle")
        .label("top")
        .i(asm::sop2("s_add_u32", O::S(12), O::S(12), O::K(16)))
        .label("middle")
        .i(asm::sop2("s_add_u32", O::S(12), O::S(12), O::K(1)))
        .i(asm::sopc("s_cmp_lt_u32", O::S(12), O::K(60)))
        .branch("s_cbranch_scc1", "top")
        .i(asm::vop1("v_mov_b32", 5, O::S(12)))
        .i(export_position([5, 5, 5, 5]))
        .end();
    for user in [0, 1] {
        let (function, outcome) = run_both_with(&vertex(code.clone()), &vertex_data(), 3, |host| {
            host.user[8] = user
        });
        assert!(ControlFlow::of(&function).reducible);
        assert_eq!(position(&outcome, 2)[0], if user == 0 { 69 } else { 68 });
    }
}

/// A vertex program over a wave of 64 or 32 lanes with user data in `s8`
/// to `s11`.
fn wave_program(wide: bool, code: Vec<u32>) -> Program {
    let wave = if wide {
        WaveSize::Wave64
    } else {
        WaveSize::Wave32
    };
    program(HardwareStage::Vertex, wave, code, &[8, 9, 10, 11])
}

/// The names of the mask-wide scalar operations for a wave size: the one
/// that saves the live mask and makes every lane live, the bitwise and, and
/// the move.
fn mask_operations(wide: bool) -> (&'static str, &'static str, &'static str) {
    if wide {
        ("s_or_saveexec_b64", "s_and_b64", "s_mov_b64")
    } else {
        ("s_or_saveexec_b32", "s_and_b32", "s_mov_b32")
    }
}

/// The section a compiler writes out for a reduction over the wave: the
/// words of `value` in the live lanes (and, with `part`, only those of
/// them whose bit is set in the mask at that register), folded with the
/// operation `name`, into `s20`. `v1` and `v2` are its scratch registers,
/// `s30` holds the saved mask and `s28` the lanes that take part.
fn reduction(name: &str, identity: u32, wide: bool, part: Option<u8>, value: u8) -> Vec<u32> {
    let (save, and, mov) = mask_operations(wide);
    let mut words = asm::sop1(save, O::S(30), O::K(-1));
    let mut mask = O::S(30);
    if let Some(part) = part {
        words.extend(asm::sop2(and, O::S(28), O::S(part), O::S(30)));
        mask = O::S(28);
    }
    // The identity goes into the lanes that take no part.
    let other = match identity {
        0 => O::K(0),
        u32::MAX => O::K(-1),
        identity => {
            words.extend(asm::vop1("v_mov_b32", 1, O::Lit(identity)));
            O::V(1)
        }
    };
    words.extend(asm::vop3(
        "v_cndmask_b32",
        1,
        [other, O::V(value), mask],
        0,
        0,
    ));
    // Pairs, fours, eights, the row, the two rows, and for 64 lanes the
    // two halves.
    for lanes in [
        [1, 0, 3, 2, 5, 4, 7, 6],
        [2, 2, 0, 0, 6, 6, 4, 4],
        [4, 4, 4, 4, 0, 0, 0, 0],
    ] {
        words.extend(asm::vop2_eight(name, 1, 1, 1, lanes));
    }
    words.extend(asm::vop2_rows(name, 1, 1, 1, 0x140, true));
    words.extend(asm::across_rows(2, 1, 0, 0));
    words.extend(asm::vop2(name, 1, O::V(2), 1));
    if wide {
        words.extend(asm::read_lane(20, 1, 32));
        words.extend(asm::vop2(name, 1, O::S(20), 1));
    }
    words.extend(asm::read_lane(20, 1, 0));
    words.extend(asm::sop1(mov, O::Exec, O::S(30)));
    words
}

/// Whether a lane is live after `v_cmpx_lt_u32 v3, v0` in a vertex program.
fn lane_is_live(lane: usize) -> bool {
    lane_word(lane, 2) < lane_word(lane, 1)
}

/// Whether a lane's bit is set by `v_cmp_lt_u32 vcc, 0x40000000, v3`.
fn lane_takes_part(lane: usize) -> bool {
    lane_word(lane, 2) > 0x4000_0000
}

#[test]
fn a_reduction_the_compiler_wrote_out_becomes_one_operation_over_the_wave() {
    // Every operation, in waves of both sizes, over the live lanes and
    // over a part of them. The float sum gets floats made from integers,
    // large enough that every addition rounds, so that the order of the
    // additions shows; the others get the lanes' raw words, NaNs and both
    // zeroes among them.
    let operations: [(&str, u32); 11] = [
        ("v_or_b32", 0),
        ("v_and_b32", u32::MAX),
        ("v_xor_b32", 0),
        ("v_add_nc_u32", 0),
        ("v_min_u32", u32::MAX),
        ("v_max_u32", 0),
        ("v_min_i32", 0x7FFF_FFFF),
        ("v_max_i32", 0x8000_0000),
        ("v_min_f32", 0x7F80_0000),
        ("v_max_f32", 0xFF80_0000),
        ("v_add_f32", 0),
    ];
    for wide in [true, false] {
        let lanes = if wide { 64 } else { 32 };
        let (_, _, mov) = mask_operations(wide);
        for (name, identity) in operations {
            for part in [None, Some(24)] {
                let value = if name == "v_add_f32" { 6 } else { 0 };
                let code = Code::new()
                    .i(asm::sop1(mov, O::S(10), O::Exec))
                    .i(asm::vop1("v_cvt_f32_u32", 6, O::V(0)))
                    .i(asm::vopc("v_cmp_lt_u32", O::Lit(0x4000_0000), 3))
                    .i(asm::sop1(mov, O::S(24), O::Vcc))
                    .i(asm::vopc("v_cmpx_lt_u32", O::V(3), 0))
                    .i(reduction(name, identity, wide, part, value))
                    // Every lane gets the result, the ones that took no
                    // part too.
                    .i(asm::sop1(mov, O::Exec, O::S(10)))
                    .i(asm::vop1("v_mov_b32", 5, O::S(20)))
                    .i(export_position([5, 0, 3, 5]))
                    .end();
                let (function, outcome) =
                    run_both(&wave_program(wide, code), &vertex_data(), lanes);
                let ops: Vec<&Op> = function
                    .blocks
                    .iter()
                    .flat_map(|block| &block.insts)
                    .map(|inst| &inst.op)
                    .collect();
                let reductions = ops
                    .iter()
                    .filter(|op| matches!(op, Op::Reduce { .. }))
                    .count();
                assert_eq!(reductions, 1, "{name}\n{function}");
                // Nothing is left that reads a lane by its number.
                assert!(
                    !ops.iter().any(|op| matches!(
                        op,
                        Op::ReadLane { .. } | Op::QuadPermute { .. } | Op::LaneBit { .. }
                    )),
                    "{name}\n{function}"
                );
                // For the integer operations the result is worked out here
                // a third time.
                let fold: Option<fn(u32, u32) -> u32> = match name {
                    "v_or_b32" => Some(|a, b| a | b),
                    "v_and_b32" => Some(|a, b| a & b),
                    "v_xor_b32" => Some(|a, b| a ^ b),
                    "v_add_nc_u32" => Some(u32::wrapping_add),
                    "v_min_u32" => Some(u32::min),
                    "v_max_u32" => Some(u32::max),
                    "v_min_i32" => Some(|a, b| (a as i32).min(b as i32) as u32),
                    "v_max_i32" => Some(|a, b| (a as i32).max(b as i32) as u32),
                    _ => None,
                };
                if let Some(fold) = fold {
                    let expected = (0..lanes)
                        .filter(|&lane| lane_is_live(lane))
                        .filter(|&lane| part.is_none() || lane_takes_part(lane))
                        .fold(identity, |folded, lane| fold(folded, lane_word(lane, 1)));
                    for lane in 0..lanes {
                        assert_eq!(position(&outcome, lane)[0], expected, "{name} lane {lane}");
                    }
                }
            }
        }
    }
}

/// The section a compiler writes out for a prefix sum: in each lane the sum
/// of `value` over the live lanes below it (and the lane's own, if
/// `inclusive`), into `v3`. `v2` and `v4` are scratch, as is `s21`.
fn prefix_sum(wide: bool, inclusive: bool, value: u8) -> Vec<u32> {
    let (save, _, mov) = mask_operations(wide);
    let add = "v_add_nc_u32";
    let mut words = asm::sop1(save, O::S(30), O::K(-1));
    words.extend(asm::vop3(
        "v_cndmask_b32",
        2,
        [O::K(0), O::V(value), O::S(30)],
        0,
        0,
    ));
    words.extend(asm::vop1("v_mov_b32", 3, O::V(2)));
    // Within each row of sixteen: shifts by one, two, four and eight.
    words.extend(asm::vop2_rows(add, 3, 3, 3, 0x111, true));
    for control in [0x112, 0x114, 0x118] {
        words.extend(asm::vop2_rows(add, 3, 3, 3, control, false));
    }
    // The upper row of each half takes the lower row's total.
    words.extend(asm::sop1("s_mov_b32", O::Exec, O::Lit(0xFFFF_0000)));
    if wide {
        words.extend(asm::sop1("s_mov_b32", O::ExecHigh, O::Lit(0xFFFF_0000)));
    }
    words.extend(asm::across_rows(4, 3, -1, 9));
    words.extend(asm::vop2(add, 3, O::V(4), 3));
    if wide {
        // The upper half takes the lower half's total.
        words.extend(asm::read_lane(21, 3, 31));
        words.extend(asm::sop1("s_mov_b32", O::Exec, O::K(0)));
        words.extend(asm::sop1("s_mov_b32", O::ExecHigh, O::K(-1)));
        words.extend(asm::vop2(add, 3, O::S(21), 3));
        words.extend(asm::sop1("s_mov_b32", O::ExecHigh, O::K(-1)));
    }
    words.extend(asm::sop1("s_mov_b32", O::Exec, O::K(-1)));
    if !inclusive {
        words.extend(asm::vop2("v_sub_nc_u32", 3, O::V(3), 2));
    }
    words.extend(asm::sop1(mov, O::Exec, O::S(30)));
    words
}

#[test]
fn a_prefix_sum_the_compiler_wrote_out_becomes_one_operation_over_the_wave() {
    for wide in [true, false] {
        let lanes = if wide { 64 } else { 32 };
        for inclusive in [false, true] {
            let code = Code::new()
                .i(asm::vopc("v_cmpx_lt_u32", O::V(3), 0))
                .i(prefix_sum(wide, inclusive, 0))
                .i(asm::vop1("v_mov_b32", 5, O::V(3)))
                .i(export_position([5, 2, 0, 3]))
                .end();
            let (function, outcome) = run_both(&wave_program(wide, code), &vertex_data(), lanes);
            let sums = function
                .blocks
                .iter()
                .flat_map(|block| &block.insts)
                .filter(|inst| matches!(inst.op, Op::PrefixSum { .. }))
                .count();
            assert_eq!(sums, 1, "{function}");
            let mut sum = 0u32;
            for lane in (0..lanes).filter(|&lane| lane_is_live(lane)) {
                let own = lane_word(lane, 1);
                let expected = if inclusive {
                    sum.wrapping_add(own)
                } else {
                    sum
                };
                // The sum, and the scratch register that holds the lane's
                // own word.
                assert_eq!(position(&outcome, lane)[..2], [expected, own]);
                sum = sum.wrapping_add(own);
            }
        }
    }
}

#[test]
fn what_a_reduction_leaves_behind_cannot_be_read_and_other_permutations_are_refused() {
    // `v2` holds the fold of the other row of sixteen when the section
    // ends, which is not something a host lane can be given.
    let code = Code::new()
        .i(reduction("v_or_b32", 0, true, None, 0))
        .i(asm::vop1("v_mov_b32", 5, O::V(2)))
        .i(export_position([5, 5, 5, 5]))
        .end();
    let error = lift(&wave_program(true, code), &vertex_data()).unwrap_err();
    assert_eq!(error.instruction, "v_mov_b32");
    assert!(error.reason.contains("whole-wave section left"), "{error}");

    // Written again, it is a register like any other.
    let code = Code::new()
        .i(reduction("v_or_b32", 0, true, None, 0))
        .i(asm::vop1("v_mov_b32", 2, O::V(3)))
        .i(asm::vop1("v_mov_b32", 5, O::V(2)))
        .i(export_position([5, 5, 5, 5]))
        .end();
    run_both(&wave_program(true, code), &vertex_data(), 64);

    // The same steps without the section around them, or with a step that
    // counts a lane twice, are not a reduction.
    let code = Code::new()
        .i(asm::vop2_eight(
            "v_or_b32",
            1,
            0,
            0,
            [1, 0, 3, 2, 5, 4, 7, 6],
        ))
        .i(export_position([1, 1, 1, 1]))
        .end();
    let error = lift(&wave_program(true, code), &vertex_data()).unwrap_err();
    assert!(error.reason.contains("eight lanes"), "{error}");
    let (save, _, mov) = mask_operations(true);
    let code = Code::new()
        .i(asm::sop1(save, O::S(30), O::K(-1)))
        .i(asm::vop3(
            "v_cndmask_b32",
            1,
            [O::K(0), O::V(0), O::S(30)],
            0,
            0,
        ))
        .i(asm::vop2_eight(
            "v_add_nc_u32",
            1,
            1,
            1,
            [1, 0, 3, 2, 5, 4, 7, 6],
        ))
        // Lane 0 would add lane 1's pair, which holds lane 0 already.
        .i(asm::vop2_eight(
            "v_add_nc_u32",
            1,
            1,
            1,
            [1, 0, 3, 2, 5, 4, 7, 6],
        ))
        .i(asm::read_lane(20, 1, 0))
        .i(asm::sop1(mov, O::Exec, O::S(30)))
        .i(asm::vop1("v_mov_b32", 5, O::S(20)))
        .i(export_position([5, 5, 5, 5]))
        .end();
    let error = lift(&wave_program(true, code), &vertex_data()).unwrap_err();
    assert!(error.reason.contains("whole-wave section left"), "{error}");

    // A lane read of a copy the section made is a step the lift has no
    // rule for: the section is then lifted like any other code, and fails
    // at its first permutation.
    let mut section = reduction("v_or_b32", 0, true, None, 0);
    let restore = section.split_off(section.len() - 1);
    section.extend(asm::vop1("v_mov_b32", 4, O::V(3)));
    section.extend(asm::read_lane(21, 4, 5));
    section.extend(restore);
    let code = Code::new()
        .i(section)
        .i(asm::vop1("v_mov_b32", 5, O::S(20)))
        .i(export_position([5, 5, 5, 5]))
        .end();
    let error = lift(&wave_program(true, code), &vertex_data()).unwrap_err();
    assert!(error.reason.contains("eight lanes"), "{error}");
}

#[test]
fn a_count_of_all_lanes_below_is_the_lanes_number() {
    // With every bit of the mask operand set the count is the lane's
    // number, whichever lanes are live; with a number, the bits of the
    // number below the lane.
    for wide in [true, false] {
        let lanes = if wide { 64 } else { 32 };
        let count = |target: u8, low: O, high: O| {
            let mut words = asm::vop3(
                "v_mbcnt_lo_u32_b32",
                u32::from(target),
                [low, O::K(0), O::K(0)],
                0,
                0,
            );
            if wide {
                words.extend(asm::vop3(
                    "v_mbcnt_hi_u32_b32",
                    u32::from(target),
                    [high, O::V(target), O::K(0)],
                    0,
                    0,
                ));
            }
            words
        };
        let code = Code::new()
            .i(asm::vopc("v_cmpx_lt_u32", O::V(3), 0))
            .i(count(5, O::K(-1), O::K(-1)))
            .i(count(6, O::S(8), O::S(9)))
            .i(export_position([5, 6, 0, 3]))
            .end();
        let (function, outcome) = run_both(&wave_program(wide, code), &vertex_data(), lanes);
        assert!(!function.has_cross_lane(), "{function}");
        let user = TestHost::new().user;
        let number = u64::from(user[8]) | if wide { u64::from(user[9]) << 32 } else { 0 };
        for lane in (0..lanes).filter(|&lane| lane_is_live(lane)) {
            let below = (number & ((1u64 << lane) - 1)).count_ones();
            assert_eq!(position(&outcome, lane)[..2], [lane as u32, below]);
        }
    }
}

#[test]
fn a_register_that_is_a_mask_on_one_path_and_a_number_on_another_gets_both_forms() {
    // After the join the register is read as a number and as a mask. On
    // the path where it holds a number, the mask is the lane's own bit of
    // it; on the path where it holds a mask, the number is the lanes' bits
    // gathered.
    let code = Code::new()
        .i(asm::sopc("s_cmp_eq_u32", O::S(8), O::K(0)))
        .branch("s_cbranch_scc1", "mask")
        .i(asm::sop1("s_mov_b32", O::S(12), O::S(9)))
        .i(asm::sop1("s_mov_b32", O::S(13), O::S(10)))
        .branch("s_branch", "join")
        .label("mask")
        .i(asm::vopc("v_cmp_lt_u32", O::V(3), 0))
        .i(asm::sop1("s_mov_b64", O::S(12), O::Vcc))
        .label("join")
        .i(asm::sop2("s_add_u32", O::S(14), O::S(12), O::K(1)))
        .i(asm::sop2("s_add_u32", O::S(15), O::S(13), O::K(1)))
        .i(asm::vop1("v_mov_b32", 5, O::S(14)))
        .i(asm::vop1("v_mov_b32", 6, O::S(15)))
        .i(asm::vop3(
            "v_cndmask_b32",
            7,
            [O::K(0), O::K(1), O::S(12)],
            0,
            0,
        ))
        .i(export_position([5, 6, 7, 0]))
        .end();
    for user in [0, 1] {
        let (_, outcome) = run_both_with(&vertex(code.clone()), &vertex_data(), 64, |host| {
            host.user[8] = user
        });
        let host = TestHost::new();
        let bits = if user == 0 {
            (0..64).fold(0u64, |bits, lane| {
                bits | u64::from(lane_is_live(lane)) << lane
            })
        } else {
            u64::from(host.user[9]) | u64::from(host.user[10]) << 32
        };
        for lane in 0..64 {
            assert_eq!(
                position(&outcome, lane)[..3],
                [
                    (bits as u32).wrapping_add(1),
                    ((bits >> 32) as u32).wrapping_add(1),
                    (bits >> lane & 1) as u32
                ],
                "user {user} lane {lane}"
            );
        }
    }

    // A known constant is not taken for a mask: its bits would be read by
    // the console's lane numbers. The register cannot be read then.
    let code = Code::new()
        .i(asm::sopc("s_cmp_eq_u32", O::S(8), O::K(0)))
        .branch("s_cbranch_scc1", "mask")
        .i(asm::sop1("s_mov_b32", O::S(12), O::K(5)))
        .i(asm::sop1("s_mov_b32", O::S(13), O::K(7)))
        .branch("s_branch", "join")
        .label("mask")
        .i(asm::vopc("v_cmp_lt_u32", O::V(3), 0))
        .i(asm::sop1("s_mov_b64", O::S(12), O::Vcc))
        .label("join")
        .i(asm::sop2("s_add_u32", O::S(14), O::S(12), O::K(1)))
        .end();
    let error = lift(&vertex(code), &vertex_data()).unwrap_err();
    assert_eq!(error.instruction, "s_add_u32");
    assert!(error.reason.contains("lane mask on one path"));
}

/// A buffer descriptor in `s20` to `s23` around the address in `s10` and
/// `s11`: records of one byte, no bound.
fn raw_descriptor() -> Vec<u32> {
    let mut words = asm::sop1("s_mov_b32", O::S(20), O::S(10));
    words.extend(asm::sop2(
        "s_or_b32",
        O::S(21),
        O::S(11),
        O::Lit(0x0001_0000),
    ));
    words.extend(asm::sop1("s_mov_b32", O::S(22), O::K(-1)));
    words.extend(asm::sop1("s_mov_b32", O::S(23), O::Lit(0x1000_0000)));
    words
}

#[test]
fn atomic_operations_change_memory_a_lane_at_a_time() {
    // Sixty-four lanes work on eight words, so every word is worked on by
    // several lanes. Each operation has its own eight words.
    let operations = [
        "add", "sub", "smin", "umin", "smax", "umax", "and", "or", "xor", "inc", "dec", "swap",
    ];
    let mut code = Code::new()
        .i(raw_descriptor())
        .i(asm::vopc("v_cmpx_lt_u32", O::V(3), 0))
        .i(asm::vop2("v_and_b32", 6, O::K(7), 0))
        .i(asm::vop2("v_lshlrev_b32", 7, O::K(2), 6));
    for (index, operation) in operations.iter().enumerate() {
        code = code.i(asm::mubuf(
            &format!("buffer_atomic_{operation}"),
            0,
            7,
            20,
            0x40 * index as u32,
            false,
            true,
        ));
    }
    let code = code
        // A compare-and-swap of the lane's word for a zero: the first lane
        // at each word wins. The new word is in v8, the comparand in v9.
        .i(asm::vop1("v_mov_b32", 8, O::V(0)))
        .i(asm::vop1("v_mov_b32", 9, O::K(0)))
        .i(asm::mubuf(
            "buffer_atomic_cmpswap",
            8,
            7,
            20,
            0x400,
            false,
            true,
        ))
        // The forms that give the old word back: each lane sees what the
        // lanes before it left.
        .i(asm::vop1("v_mov_b32", 10, O::V(3)))
        .i(asm::returning(asm::mubuf(
            "buffer_atomic_add",
            10,
            7,
            20,
            0x440,
            false,
            true,
        )))
        .i(asm::vop1("v_mov_b32", 11, O::V(3)))
        .i(asm::returning(asm::mubuf(
            "buffer_atomic_cmpswap",
            11,
            7,
            20,
            0x480,
            false,
            true,
        )))
        // By address: the base in s10 and s11, the offset in v7.
        .i(asm::global("global_atomic_umax", 0, 7, 0, 10, 0x4C0))
        .i(asm::returning(asm::global(
            "global_atomic_add",
            13,
            7,
            3,
            10,
            0x500,
        )))
        .i(export_position([10, 11, 13, 0]))
        .end();
    let view = 0x34_0000_0100u64;
    let prepare = |host: &mut TestHost| {
        host.user[10] = view as u32;
        host.user[11] = (view >> 32) as u32;
        // The words the compare-and-swaps work on start as zero.
        host.write_words(view + 0x400, &[0; 8]);
        host.write_words(view + 0x480, &[0; 8]);
    };
    let (function, outcome) = run_both_with(&vertex(code), &vertex_data(), 64, prepare);
    assert!(!function.has_cross_lane());
    let mut host = TestHost::new();
    prepare(&mut host);
    let stored = |at: u64| {
        let bytes: Vec<u8> = (0..4).map(|byte| outcome.stores[&(at + byte)]).collect();
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    };
    let lanes_at = |word: u64| {
        (0..64).filter(move |&lane| lane_is_live(lane) && u64::from(lane_word(lane, 1) & 7) == word)
    };
    for word in 0..8u64 {
        if lanes_at(word).next().is_none() {
            continue;
        }
        // The sum: what was there and every lane's word.
        let before = host.load(view + 4 * word, 32);
        let sum = lanes_at(word).fold(before, |sum, lane| sum.wrapping_add(lane_word(lane, 1)));
        assert_eq!(stored(view + 4 * word), sum);
        // The swap: the last lane's word.
        let last = lanes_at(word).next_back().unwrap();
        assert_eq!(stored(view + 0x40 * 11 + 4 * word), lane_word(last, 1));
        // The compare-and-swap: the first lane's word that is not zero.
        let swapped = lanes_at(word).fold(
            0,
            |held, lane| {
                if held == 0 { lane_word(lane, 1) } else { held }
            },
        );
        assert_eq!(stored(view + 0x400 + 4 * word), swapped);
        // The sum that gives the old word back: a running sum.
        let mut running = host.load(view + 0x440 + 4 * word, 32);
        for lane in lanes_at(word) {
            assert_eq!(position(&outcome, lane)[0], running);
            running = running.wrapping_add(lane_word(lane, 2));
        }
    }
}

/// A compute program of one wave of 64 threads with the given amount of
/// group-shared memory and user data in `s4` to `s7`.
fn shared_program(code: Vec<u32>, shared_bytes: u32) -> (Program, StageData) {
    let program = program(
        HardwareStage::Compute,
        WaveSize::Wave64,
        code,
        &[4, 5, 6, 7],
    );
    let data = StageData::Compute(ComputeData {
        // Eight user registers, one thread component, the shared memory
        // in units of 512 bytes.
        resources: 8 << 1 | (shared_bytes / 512) << 15,
        group_size: [64, 1, 1],
    });
    (program, data)
}

#[test]
fn lanes_see_what_other_lanes_wrote_to_the_shared_memory() {
    // v0 is the thread's number. Each thread writes a word at its own
    // place, then reads the place of the thread opposite: what it reads was
    // written by another lane at an earlier instruction.
    let code = Code::new()
        // A descriptor for the results: s20 to s23 around s6 and s7.
        .i(asm::sop1("s_mov_b32", O::S(20), O::S(6)))
        .i(asm::sop2(
            "s_or_b32",
            O::S(21),
            O::S(7),
            O::Lit(0x0001_0000),
        ))
        .i(asm::sop1("s_mov_b32", O::S(22), O::K(-1)))
        .i(asm::sop1("s_mov_b32", O::S(23), O::Lit(0x1000_0000)))
        .i(asm::vop2("v_lshlrev_b32", 6, O::K(2), 0))
        .i(asm::vop3(
            "v_mul_lo_u32",
            7,
            [O::S(4), O::V(0), O::K(0)],
            0,
            0,
        ))
        .i(asm::vop2("v_sub_nc_u32", 8, O::K(63), 0))
        .i(asm::vop2("v_lshlrev_b32", 8, O::K(2), 8))
        .i(asm::vop1("v_mov_b32", 12, O::K(0)))
        .i(asm::ds("ds_write_b32", 0, 6, [7, 0], [0, 0]))
        .i(asm::ds("ds_read_b32", 16, 8, [0, 0], [0, 0]))
        // Two words at once, 64 and 130 words on.
        .i(asm::ds("ds_write2_b32", 0, 6, [16, 0], [64, 130]))
        .i(asm::ds("ds_read2_b32", 17, 8, [0, 0], [64, 130]))
        // Atomic operations of every lane on one word at byte 0x320, and
        // the ones that give the old word back at 0x330 and on.
        .i(asm::ds("ds_add_u32", 0, 12, [0, 0], [0x20, 0x03]))
        .i(asm::ds("ds_min_u32", 0, 12, [7, 0], [0x24, 0x03]))
        .i(asm::ds("ds_max_i32", 0, 12, [7, 0], [0x28, 0x03]))
        .i(asm::ds("ds_or_b32", 0, 12, [7, 0], [0x2C, 0x03]))
        .i(asm::ds("ds_add_rtn_u32", 19, 12, [7, 0], [0x30, 0x03]))
        .i(asm::ds("ds_wrxchg_rtn_b32", 20, 12, [7, 0], [0x34, 0x03]))
        // A compare-and-store of the thread's number for a zero: the
        // comparand is the first data register.
        .i(asm::ds("ds_cmpst_rtn_b32", 21, 12, [12, 7], [0x38, 0x03]))
        .i(asm::sopp("s_barrier", 0))
        .i(asm::ds("ds_read_b32", 22, 12, [0, 0], [0x20, 0x03]))
        .i(asm::ds("ds_read_b32", 23, 12, [0, 0], [0x24, 0x03]))
        // Bytes and halves, at odd places; a signed half read.
        .i(asm::ds("ds_write_b8", 0, 0, [7, 0], [0x01, 0x05]))
        .i(asm::ds("ds_write_b16", 0, 6, [7, 0], [0x01, 0x06]))
        .i(asm::ds("ds_read_u8", 24, 0, [0, 0], [0x01, 0x05]))
        .i(asm::ds("ds_read_i16", 25, 6, [0, 0], [0x01, 0x06]))
        // In units of 64 words; and past the end of the memory, where a
        // write is dropped and a read gives zero.
        .i(asm::ds("ds_write2st64_b32", 0, 6, [7, 16], [7, 8]))
        .i(asm::ds("ds_read2st64_b32", 26, 8, [0, 0], [7, 8]))
        .i(asm::ds("ds_write_b32", 0, 6, [7, 0], [0xFE, 0xFF]))
        .i(asm::ds("ds_read_b32", 28, 6, [0, 0], [0xFE, 0xFF]))
        // Everything read goes to memory, 64 bytes for each thread.
        .i(asm::vop2("v_lshlrev_b32", 9, O::K(6), 0))
        .i(asm::mubuf(
            "buffer_store_dwordx4",
            16,
            9,
            20,
            0,
            false,
            true,
        ))
        .i(asm::mubuf(
            "buffer_store_dwordx4",
            20,
            9,
            20,
            16,
            false,
            true,
        ))
        .i(asm::mubuf(
            "buffer_store_dwordx4",
            24,
            9,
            20,
            32,
            false,
            true,
        ))
        .i(asm::mubuf("buffer_store_dword", 28, 9, 20, 48, false, true))
        .end();
    let (program, data) = shared_program(code, 4096);
    let view = 0x21_0000_4000u64;
    let prepare = |host: &mut TestHost| {
        host.user[6] = view as u32;
        host.user[7] = (view >> 32) as u32;
    };
    let (function, outcome) = run_both_with(&program, &data, 64, prepare);
    assert_eq!(function.interface.shared_bytes, 4096);
    let barriers = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter(|inst| matches!(inst.op, Op::Barrier))
        .count();
    assert_eq!(barriers, 1);
    let stored = |lane: u64, word: u64| {
        let at = view + 64 * lane + 4 * word;
        let bytes: Vec<u8> = (0..4).map(|byte| outcome.stores[&(at + byte)]).collect();
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    };
    let scale = TestHost::new().user[4];
    let written = |lane: u64| scale.wrapping_mul(lane as u32);
    let total = (0..64u32).sum::<u32>();
    for lane in 0..64u64 {
        // The word of the thread opposite.
        assert_eq!(stored(lane, 0), written(63 - lane), "lane {lane}");
        // The first of the two words is what the opposite thread read
        // first, which is this thread's own word.
        assert_eq!(stored(lane, 1), written(lane), "lane {lane}");
        // The sum of all thread numbers, read after the barrier.
        assert_eq!(stored(lane, 6), total, "lane {lane}");
        // The running sum of the words the lanes before added.
        let running = (0..lane).fold(0u32, |sum, other| sum.wrapping_add(written(other)));
        assert_eq!(stored(lane, 3), running, "lane {lane}");
        // Past the end of the memory.
        assert_eq!(stored(lane, 12), 0, "lane {lane}");
    }
}

#[test]
fn shared_memory_outside_a_compute_program_is_refused() {
    let code = Code::new()
        .i(asm::ds("ds_write_b32", 0, 0, [3, 0], [0, 0]))
        .i(export_position([0, 0, 0, 0]))
        .end();
    let error = lift(&vertex(code), &vertex_data()).unwrap_err();
    assert_eq!(error.instruction, "ds_write_b32");
    assert!(error.reason.contains("outside a compute program"));
}

#[test]
fn a_register_pair_is_shifted_as_one_64_bit_value() {
    for name in ["v_lshlrev_b64", "v_lshrrev_b64", "v_ashrrev_i64"] {
        // The pair v6:v7 holds the vertex and instance numbers. Shifted by
        // a constant, by 32 and more, and by a count that differs per lane.
        let code = Code::new()
            .i(asm::vop1("v_mov_b32", 6, O::V(0)))
            .i(asm::vop1("v_mov_b32", 7, O::V(3)))
            .i(asm::vop3(name, 8, [O::K(6), O::V(6), O::K(0)], 0, 0))
            .i(asm::vop3(name, 10, [O::K(37), O::V(6), O::K(0)], 0, 0))
            .i(asm::vop3(name, 12, [O::V(0), O::V(6), O::K(0)], 0, 0))
            .i(export_position([8, 9, 10, 11]))
            .i(asm::exp(32, [12, 13, 6, 7], false))
            .end();
        let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
        for lane in 0..64 {
            let value = u64::from(lane_word(lane, 1)) | u64::from(lane_word(lane, 2)) << 32;
            let shift = |by: u32| match name {
                "v_lshlrev_b64" => value << by,
                "v_lshrrev_b64" => value >> by,
                _ => ((value as i64) >> by) as u64,
            };
            let (by_6, by_37) = (shift(6), shift(37));
            assert_eq!(
                position(&outcome, lane),
                [
                    by_6 as u32,
                    (by_6 >> 32) as u32,
                    by_37 as u32,
                    (by_37 >> 32) as u32
                ],
                "{name} lane {lane}"
            );
            let by_lane = shift(lane_word(lane, 1) & 63);
            let (_, values) = outcome.exports[&lane][1];
            assert_eq!(values[0], Some(by_lane as u32));
            assert_eq!(values[1], Some((by_lane >> 32) as u32));
        }
    }
}

#[test]
fn a_float_is_rounded_into_one_byte_of_a_word() {
    // The vertex number as a float, scaled so that the lanes hold values
    // below 0, above 255 and in between, goes into each byte in turn of
    // the instance number.
    let code = Code::new()
        .i(asm::vop1("v_cvt_f32_i32", 6, O::V(0)))
        .i(asm::vop2("v_mul_f32", 6, O::Lit(0x3280_0000), 6))
        .i(asm::vop3(
            "v_cvt_pk_u8_f32",
            8,
            [O::V(6), O::K(0), O::V(3)],
            0,
            0,
        ))
        .i(asm::vop3(
            "v_cvt_pk_u8_f32",
            9,
            [O::V(6), O::K(1), O::V(3)],
            0,
            0,
        ))
        .i(asm::vop3(
            "v_cvt_pk_u8_f32",
            10,
            [O::V(0), O::K(2), O::V(3)],
            0,
            0,
        ))
        .i(asm::vop3(
            "v_cvt_pk_u8_f32",
            11,
            [O::V(6), O::K(7), O::V(3)],
            0,
            0,
        ))
        .i(export_position([8, 9, 10, 11]))
        .end();
    let (_, outcome) = run_both(&vertex(code), &vertex_data(), 64);
    let mut inside = 0;
    for lane in 0..64 {
        let scaled = lane_word(lane, 1) as i32 as f32 * f32::from_bits(0x3280_0000);
        let byte = scaled.round_ties_even().clamp(0.0, 255.0) as u32;
        inside += u32::from(byte > 0 && byte < 255);
        let old = lane_word(lane, 2);
        let words = position(&outcome, lane);
        assert_eq!(words[0], old & !0xFF | byte, "lane {lane}");
        assert_eq!(words[1], old & !0xFF00 | byte << 8, "lane {lane}");
        // The place is the low two bits of its operand: 7 is byte 3.
        assert_eq!(words[3], old & !0xFF00_0000 | byte << 24, "lane {lane}");
    }
    assert!(inside > 4, "the lanes do not cover the bytes in between");
}

#[test]
fn a_number_kept_in_the_vector_condition_register_is_a_number() {
    // A compiler that has run out of scalar registers keeps numbers in
    // `VCC`. Afterwards a comparison makes it a lane mask again.
    for wide in [true, false] {
        let lanes = if wide { 64 } else { 32 };
        let code = Code::new()
            .i(asm::sop2("s_add_i32", O::Vcc, O::S(8), O::K(16)))
            .i(asm::vop1("v_mov_b32", 5, O::Vcc))
            .i(asm::sop2("s_lshl_b32", O::S(12), O::Vcc, O::K(2)))
            .i(asm::vop1("v_mov_b32", 6, O::S(12)))
            .i(asm::vopc("v_cmp_lt_u32", O::V(3), 0))
            .i(asm::vop2("v_cndmask_b32", 7, O::K(0), 0))
            .i(export_position([5, 6, 7, 0]))
            .end();
        let (function, outcome) = run_both(&wave_program(wide, code), &vertex_data(), lanes);
        assert!(!function.has_cross_lane(), "{function}");
        let number = TestHost::new().user[8].wrapping_add(16);
        for lane in 0..lanes {
            let chosen = if lane_is_live(lane) {
                lane_word(lane, 1)
            } else {
                0
            };
            assert_eq!(
                position(&outcome, lane)[..3],
                [number, number << 2, chosen],
                "lane {lane}"
            );
        }
    }
}
