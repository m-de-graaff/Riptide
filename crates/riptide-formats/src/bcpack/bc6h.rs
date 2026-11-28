//! BC6H blocks.
//!
//! A BC6H block has one of fourteen modes. A mode fixes how many bits the
//! first endpoint has per channel (the *base*), how many the other endpoints
//! have (which the block format stores as differences, the *deltas*), and
//! whether the block has two regions (modes 1 to 10: four endpoints, 3-bit
//! indices, a partition number) or one (modes 11 to 14: two endpoints, 4-bit
//! indices). The mode numbers, their bit counts and the order of the fields
//! in a block are those of Microsoft's public description of the format.
//!
//! The packed form treats every field as an opaque number. It keeps each one
//! aligned at the top of sixteen bits, with the top bit flipped, so that a
//! field of one width can continue a field of another width in the referenced
//! block.

use super::bits::{BlockBits, braid, pick_steps, rebase, step16};
use super::{Error, Format, RECORD, bc7};

/// What a later block can refer to.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct State {
    /// The mode, 1 to 14, which gives the width of the indices. Dictionary
    /// entries may hold other values; those have no indices to continue.
    pub(super) mode: u8,
    /// Four endpoints of red, green and blue, top-aligned.
    pub(super) ends: [[u16; 3]; 4],
    /// The indices as signed numbers around the middle of their range.
    pub(super) picks: [i16; 16],
}

/// A run of bits of a block's header.
#[derive(Clone, Copy)]
struct Field {
    /// 0 red, 1 green, 2 blue; 3 for the partition number.
    channel: u8,
    endpoint: u8,
    /// The lowest bit of the endpoint field that the run holds.
    low: u8,
    count: u8,
}

/// The most runs a header has.
const RUNS: usize = 24;

struct Mode {
    /// The mode bits a block starts with, and how many they are.
    code: u8,
    code_bits: u8,
    /// Bits of the first endpoint's channels.
    base: u8,
    /// Bits of the other endpoints' red, green and blue.
    delta: [u8; 3],
    runs: [Field; RUNS],
    run_count: usize,
}

/// Reads a header layout written the way the public format tables write it:
/// `rw0-9` is bits 0 to 9 of the red of the first endpoint (`w`, then `x`,
/// `y`, `z`), `gz4` is bit 4 of the green of the fourth, `d` is the five
/// bits of the partition number. Runs are listed from the block's low bits
/// up, after the mode bits.
const fn mode(code: u8, code_bits: u8, base: u8, delta: [u8; 3], layout: &str) -> Mode {
    let text = layout.as_bytes();
    let none = Field {
        channel: 0,
        endpoint: 0,
        low: 0,
        count: 0,
    };
    let mut runs = [none; RUNS];
    let mut run_count = 0;
    let mut at = 0;
    while at < text.len() {
        if text[at] == b' ' {
            at += 1;
            continue;
        }
        if text[at] == b'd' {
            runs[run_count] = Field {
                channel: 3,
                endpoint: 0,
                low: 0,
                count: 5,
            };
            run_count += 1;
            at += 1;
            continue;
        }
        let channel = match text[at] {
            b'r' => 0,
            b'g' => 1,
            b'b' => 2,
            _ => panic!("a layout names a channel that does not exist"),
        };
        let endpoint = match text[at + 1] {
            b'w' => 0,
            b'x' => 1,
            b'y' => 2,
            b'z' => 3,
            _ => panic!("a layout names an endpoint that does not exist"),
        };
        at += 2;
        let mut low = 0;
        while at < text.len() && text[at].is_ascii_digit() {
            low = low * 10 + (text[at] - b'0');
            at += 1;
        }
        let mut high = low;
        if at < text.len() && text[at] == b'-' {
            at += 1;
            high = 0;
            while at < text.len() && text[at].is_ascii_digit() {
                high = high * 10 + (text[at] - b'0');
                at += 1;
            }
        }
        runs[run_count] = Field {
            channel,
            endpoint,
            low,
            count: high - low + 1,
        };
        run_count += 1;
    }
    Mode {
        code,
        code_bits,
        base,
        delta,
        runs,
        run_count,
    }
}

/// The fourteen modes, in the order of the public tables.
///
/// One thing differs from those tables: in modes 13 and 14 they list the
/// high bits of the first endpoint from the top down, and here those bits
/// run upwards like every other run. The packed form numbers the bits of
/// that field in the order the block stores them, so the block comes out the
/// same.
static MODES: [Mode; 14] = [
    mode(
        0b00,
        2,
        10,
        [5, 5, 5],
        "gy4 by4 bz4 rw0-9 gw0-9 bw0-9 rx0-4 gz4 gy0-3 gx0-4 bz0 gz0-3 bx0-4 bz1 by0-3 ry0-4 \
         bz2 rz0-4 bz3 d",
    ),
    mode(
        0b01,
        2,
        7,
        [6, 6, 6],
        "gy5 gz4 gz5 rw0-6 bz0 bz1 by4 gw0-6 by5 bz2 gy4 bw0-6 bz3 bz5 bz4 rx0-5 gy0-3 gx0-5 \
         gz0-3 bx0-5 by0-3 ry0-5 rz0-5 d",
    ),
    mode(
        0b00010,
        5,
        11,
        [5, 4, 4],
        "rw0-9 gw0-9 bw0-9 rx0-4 rw10 gy0-3 gx0-3 gw10 bz0 gz0-3 bx0-3 bw10 bz1 by0-3 ry0-4 \
         bz2 rz0-4 bz3 d",
    ),
    mode(
        0b00110,
        5,
        11,
        [4, 5, 4],
        "rw0-9 gw0-9 bw0-9 rx0-3 rw10 gz4 gy0-3 gx0-4 gw10 gz0-3 bx0-3 bw10 bz1 by0-3 ry0-3 \
         bz0 bz2 rz0-3 gy4 bz3 d",
    ),
    mode(
        0b01010,
        5,
        11,
        [4, 4, 5],
        "rw0-9 gw0-9 bw0-9 rx0-3 rw10 by4 gy0-3 gx0-3 gw10 bz0 gz0-3 bx0-4 bw10 by0-3 ry0-3 \
         bz1 bz2 rz0-3 bz4 bz3 d",
    ),
    mode(
        0b01110,
        5,
        9,
        [5, 5, 5],
        "rw0-8 by4 gw0-8 gy4 bw0-8 bz4 rx0-4 gz4 gy0-3 gx0-4 bz0 gz0-3 bx0-4 bz1 by0-3 ry0-4 \
         bz2 rz0-4 bz3 d",
    ),
    mode(
        0b10010,
        5,
        8,
        [6, 5, 5],
        "rw0-7 gz4 by4 gw0-7 bz2 gy4 bw0-7 bz3 bz4 rx0-5 gy0-3 gx0-4 bz0 gz0-3 bx0-4 bz1 \
         by0-3 ry0-5 rz0-5 d",
    ),
    mode(
        0b10110,
        5,
        8,
        [5, 6, 5],
        "rw0-7 bz0 by4 gw0-7 gy5 gy4 bw0-7 gz5 bz4 rx0-4 gz4 gy0-3 gx0-5 gz0-3 bx0-4 bz1 \
         by0-3 ry0-4 bz2 rz0-4 bz3 d",
    ),
    mode(
        0b11010,
        5,
        8,
        [5, 5, 6],
        "rw0-7 bz1 by4 gw0-7 by5 gy4 bw0-7 bz5 bz4 rx0-4 gz4 gy0-3 gx0-4 bz0 gz0-3 bx0-5 \
         by0-3 ry0-4 bz2 rz0-4 bz3 d",
    ),
    mode(
        0b11110,
        5,
        6,
        [6, 6, 6],
        "rw0-5 gz4 bz0 bz1 by4 gw0-5 gy5 by5 bz2 gy4 bw0-5 gz5 bz3 bz5 bz4 rx0-5 gy0-3 gx0-5 \
         gz0-3 bx0-5 by0-3 ry0-5 rz0-5 d",
    ),
    mode(
        0b00011,
        5,
        10,
        [10, 10, 10],
        "rw0-9 gw0-9 bw0-9 rx0-9 gx0-9 bx0-9",
    ),
    mode(
        0b00111,
        5,
        11,
        [9, 9, 9],
        "rw0-9 gw0-9 bw0-9 rx0-8 rw10 gx0-8 gw10 bx0-8 bw10",
    ),
    mode(
        0b01011,
        5,
        12,
        [8, 8, 8],
        "rw0-9 gw0-9 bw0-9 rx0-7 rw10-11 gx0-7 gw10-11 bx0-7 bw10-11",
    ),
    mode(
        0b01111,
        5,
        16,
        [4, 4, 4],
        "rw0-9 gw0-9 bw0-9 rx0-3 rw10-15 gx0-3 gw10-15 bx0-3 bw10-15",
    ),
];

/// The width of the indices of a mode; 0 for a number that is no mode.
fn pick_bits(mode: u8) -> u8 {
    match mode {
        1..=10 => 3,
        11..=14 => 4,
        _ => 0,
    }
}

/// BC6H. Sixteen lanes, two symbols each per block: the references; red,
/// green and blue of the first endpoint; of the second; of the third and
/// fourth together; four lanes of index symbols; the mode (less one) and the
/// partition number.
pub(super) struct Bc6h;

impl Format for Bc6h {
    type State = State;
    const SYMBOL_BITS: &'static [u8] = &[4, 4, 8, 8, 8, 5, 5, 5, 6, 6, 6, 8, 8, 8, 8, 5];
    const PER: usize = 2;
    const BLOCK: usize = 16;

    fn block(
        &self,
        s: &[u8; RECORD],
        ends: &State,
        picks: &State,
        out: &mut [u8],
    ) -> Result<State, Error> {
        let number = s[30].wrapping_add(1);
        let Some(mode) = usize::from(number)
            .checked_sub(1)
            .and_then(|n| MODES.get(n))
        else {
            return Err(Error::Bc6hMode);
        };
        let two_regions = number <= 10;
        let partition = s[31] & 31;
        let base_shift = 16 - u32::from(mode.base);
        let delta_shift = mode.delta.map(|bits| 16 - u32::from(bits));
        // A residual is added at the field's own width; whatever the
        // referenced field had below that is cut off.
        let add =
            |from: u16, step: u16, shift: u32| from.wrapping_add(step << shift) & (0xffff << shift);

        let mut state = State {
            mode: number,
            ..State::default()
        };
        for c in 0..3 {
            // The first two endpoints spread each residual over two symbols.
            let first = step16(braid(s[4 + 2 * c], s[5 + 2 * c]));
            let second = step16(braid(s[10 + 2 * c], s[11 + 2 * c]));
            state.ends[0][c] = add(ends.ends[0][c], first, base_shift);
            state.ends[1][c] = add(ends.ends[1][c], second, delta_shift[c]);
            if two_regions {
                let third = step16(u16::from(s[16 + 2 * c]));
                let fourth = step16(u16::from(s[17 + 2 * c]));
                state.ends[2][c] = add(ends.ends[2][c], third, delta_shift[c]);
                state.ends[3][c] = add(ends.ends[3][c], fourth, delta_shift[c]);
            } else {
                // A block of one region has no second pair. What a later
                // block finds there is the first pair again, the first
                // endpoint moved to the width of the others.
                state.ends[2][c] = (state.ends[0][c] >> base_shift) << delta_shift[0];
                state.ends[3][c] = state.ends[1][c];
            }
        }

        let bits = pick_bits(number);
        let steps = pick_steps(&[s[22], s[23], s[24], s[25], s[26], s[27], s[28], s[29]]);
        let from_bits = pick_bits(picks.mode);
        for (pixel, step) in steps.into_iter().enumerate() {
            state.picks[pixel] = rebase(step, bits, picks.picks[pixel], from_bits);
        }

        let mut block = BlockBits::default();
        block.put(mode.code.into(), mode.code_bits.into());
        for run in &mode.runs[..mode.run_count] {
            if run.channel == 3 {
                block.put(partition.into(), 5);
                continue;
            }
            let (endpoint, channel) = (usize::from(run.endpoint), usize::from(run.channel));
            let shift = if endpoint == 0 {
                base_shift
            } else {
                delta_shift[channel]
            };
            let field = (state.ends[endpoint][channel] ^ 0x8000) >> shift;
            block.put(u64::from(field >> run.low), run.count.into());
        }
        // The first pixel of each region stores its index one bit short.
        let anchor = if two_regions {
            usize::from(bc7::SECOND_OF_TWO[usize::from(partition)])
        } else {
            0
        };
        for (pixel, &pick) in state.picks.iter().enumerate() {
            let short = pixel == 0 || pixel == anchor;
            let index = (pick as u16).wrapping_add(1 << (bits - 1));
            block.put(index.into(), u32::from(bits) - u32::from(short));
        }
        out.copy_from_slice(&block.bytes());
        Ok(state)
    }
}

#[cfg(test)]
pub(super) mod layout {
    //! The layouts, for the test packer, which takes blocks apart with them.

    /// The mode whose bits a block starts with: its number, 1 to 14.
    pub(in super::super) fn mode_of(block: u128) -> Option<u8> {
        super::MODES
            .iter()
            .position(|m| block & ((1 << m.code_bits) - 1) == u128::from(m.code))
            .map(|n| n as u8 + 1)
    }

    /// The mode bits of a mode, and how many they are.
    pub(in super::super) fn code(mode: u8) -> (u8, u8) {
        let m = &super::MODES[usize::from(mode) - 1];
        (m.code, m.code_bits)
    }

    /// The bits of the base and of the three deltas of a mode.
    pub(in super::super) fn widths(mode: u8) -> (u8, [u8; 3]) {
        let m = &super::MODES[usize::from(mode) - 1];
        (m.base, m.delta)
    }

    /// The header fields of a block: endpoint by channel, and the partition.
    pub(in super::super) fn fields(mode: u8, block: u128) -> ([[u16; 3]; 4], u8) {
        let m = &super::MODES[usize::from(mode) - 1];
        let mut at = u32::from(m.code_bits);
        let mut ends = [[0u16; 3]; 4];
        let mut partition = 0;
        for run in &m.runs[..m.run_count] {
            let value = (block >> at) as u16 & ((1u32 << run.count) - 1) as u16;
            if run.channel == 3 {
                partition = value as u8;
            } else {
                ends[usize::from(run.endpoint)][usize::from(run.channel)] |= value << run.low;
            }
            at += u32::from(run.count);
        }
        (ends, partition)
    }

    /// Bits of the header, mode bits included.
    pub(in super::super) fn header_bits(mode: u8) -> u32 {
        let m = &super::MODES[usize::from(mode) - 1];
        u32::from(m.code_bits)
            + m.runs[..m.run_count]
                .iter()
                .map(|r| u32::from(r.count))
                .sum::<u32>()
    }
}
