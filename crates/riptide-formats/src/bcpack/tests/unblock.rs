//! Blocks back to records, for tests only: the inverse of each format's
//! `block`, given the blocks a record refers to.
//!
//! The references are chosen at random, not well: the aim is to reach every
//! path of the decoder, not to compress.

use super::super::bc6h::{self, Bc6h};
use super::super::bc7::{self, Bc7, Control};
use super::super::bits::{braid, rebase};
use super::super::plain::{Bc1, Bc3, Bc4, Bc5, Colour, Ramp};
use super::super::{DICTIONARY, Format, HISTORY, RECORD, referred};
use super::pack::Rng;

/// The symbol whose signed step is `residual` modulo `2^bits`.
fn code(residual: u32, bits: u32) -> u32 {
    let mask = ((1u64 << bits) - 1) as u32;
    let residual = residual & mask;
    if residual < 1 << (bits - 1) {
        2 * residual
    } else {
        2 * (mask - residual) + 1
    }
}

/// The bits at the even positions of `value`.
fn evens(value: u16) -> u8 {
    (0..8).fold(0, |acc, k| acc | ((value >> (2 * k) & 1) as u8) << k)
}

/// Reads a block's fields from its low bits up.
struct Reader(u128);

impl Reader {
    fn take(&mut self, count: u32) -> u32 {
        let value = (self.0 & ((1u128 << count) - 1)) as u32;
        self.0 >>= count;
        value
    }
}

/// A format whose blocks can be turned back into records.
pub(crate) trait Unblock: Format {
    /// The record that makes `block` out of the given references. The
    /// reference symbols themselves are left for the caller.
    fn undo(&self, block: &[u8], ends: &Self::State, picks: &Self::State) -> [u8; RECORD];
}

fn colour_symbols(
    block: &[u8],
    rank: &[u8; 256],
    ends: &Colour,
    picks: &Colour,
) -> ([u8; 6], [u8; 4]) {
    let first = u16::from_le_bytes([block[0], block[1]]);
    let second = u16::from_le_bytes([block[2], block[3]]);
    let selectors = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    let target = [
        first >> 11,
        first >> 5 & 63,
        first & 31,
        second >> 11,
        second >> 5 & 63,
        second & 31,
    ];
    let mut end_symbols = [0u8; 6];
    for (k, bits) in [5, 6, 5, 5, 6, 5].into_iter().enumerate() {
        let residual = u32::from(target[k]).wrapping_sub(ends.ends[k].into());
        end_symbols[k] = code(residual, bits) as u8;
    }
    let position = |pixel: usize| match selectors >> (2 * pixel) & 3 {
        0 => 0u8,
        1 => 3,
        stored => stored as u8 - 1,
    };
    let mut quad_symbols = [0u8; 4];
    for (quad, symbol) in quad_symbols.iter_mut().enumerate() {
        let corner = (quad >> 1) * 8 + (quad & 1) * 2;
        let mut steps = 0u8;
        for (k, pixel) in [0, 1, 4, 5].into_iter().enumerate() {
            let at = corner + pixel;
            steps |= (position(at).wrapping_sub(picks.picks[at]) & 3) << (2 * k);
        }
        *symbol = rank
            .iter()
            .position(|&r| r == steps)
            .expect("the quad rank table lacks a combination") as u8;
    }
    (end_symbols, quad_symbols)
}

fn ramp_symbols(block: &[u8], rank: &[u8; 64], ends: &Ramp, picks: &Ramp) -> ([u8; 2], [u8; 8]) {
    let whole = u64::from_le_bytes(block[..8].try_into().unwrap());
    let mut end_symbols = [0u8; 2];
    for (k, symbol) in end_symbols.iter_mut().enumerate() {
        let target = block[k] ^ 0x80;
        *symbol = code(target.wrapping_sub(ends.ends[k]).into(), 8) as u8;
    }
    let position = |pixel: usize| match (whole >> (16 + 3 * pixel)) as u8 & 7 {
        0 => 0u8,
        1 => 7,
        stored => stored - 1,
    };
    let mut pair_symbols = [0u8; 8];
    for (pair, symbol) in pair_symbols.iter_mut().enumerate() {
        let step = |k: usize| position(2 * pair + k).wrapping_sub(picks.picks[2 * pair + k]) & 7;
        let steps = step(0) | step(1) << 4;
        *symbol = rank
            .iter()
            .position(|&r| r == steps)
            .expect("the pair rank table lacks a combination") as u8;
    }
    (end_symbols, pair_symbols)
}

impl Unblock for Bc1<'_> {
    fn undo(&self, block: &[u8], ends: &Colour, picks: &Colour) -> [u8; RECORD] {
        let (end_symbols, quads) = colour_symbols(block, self.0, ends, picks);
        let mut s = [0u8; RECORD];
        s[2..8].copy_from_slice(&end_symbols);
        s[8..12].copy_from_slice(&quads);
        s
    }
}

impl Unblock for Bc4<'_> {
    fn undo(&self, block: &[u8], ends: &Ramp, picks: &Ramp) -> [u8; RECORD] {
        let (end_symbols, pairs) = ramp_symbols(block, self.0, ends, picks);
        let mut s = [0u8; RECORD];
        s[2..4].copy_from_slice(&end_symbols);
        s[4..12].copy_from_slice(&pairs);
        s
    }
}

impl Unblock for Bc3<'_> {
    fn undo(&self, block: &[u8], ends: &Self::State, picks: &Self::State) -> [u8; RECORD] {
        let (alpha_ends, pairs) = ramp_symbols(&block[..8], self.alpha, &ends.0, &picks.0);
        let (c, quads) = colour_symbols(&block[8..], self.colour, &ends.1, &picks.1);
        let mut s = [0u8; RECORD];
        s[4..6].copy_from_slice(&alpha_ends);
        s[6..14].copy_from_slice(&pairs);
        s[14..20].copy_from_slice(&[c[0], c[3], c[1], c[4], c[2], c[5]]);
        s[20..24].copy_from_slice(&quads);
        s
    }
}

impl Unblock for Bc5<'_> {
    fn undo(&self, block: &[u8], ends: &Self::State, picks: &Self::State) -> [u8; RECORD] {
        let mut s = [0u8; RECORD];
        for half in 0..2 {
            let (end_symbols, pairs) =
                ramp_symbols(&block[8 * half..], self.0, &ends[half], &picks[half]);
            s[4 + 2 * half..][..2].copy_from_slice(&end_symbols);
            s[8 + 8 * half..][..8].copy_from_slice(&pairs);
        }
        s
    }
}

/// The eight symbols of sixteen index residuals. `target` is each index as
/// the decoder must rebuild it, modulo its width.
fn pick_symbols(target: [u32; 16], bits: u8, from: &[i16; 16], from_bits: u8) -> [u8; 8] {
    let mut codes = [0u8; 16];
    for pixel in 0..16 {
        // What the reference alone gives; the residual is the rest.
        let base = rebase(0, bits, from[pixel], from_bits) as u32;
        codes[pixel] = code(target[pixel].wrapping_sub(base), bits.into()) as u8;
    }
    std::array::from_fn(|pair| {
        let (left, right) = (codes[2 * pair], codes[2 * pair + 1]);
        if pair < 6 {
            braid(right, left) as u8
        } else {
            braid(left, right) as u8
        }
    })
}

impl Unblock for Bc6h {
    fn undo(&self, block: &[u8], ends: &bc6h::State, picks: &bc6h::State) -> [u8; RECORD] {
        let block = u128::from_le_bytes(block.try_into().unwrap());
        let mode = bc6h::layout::mode_of(block).expect("not a BC6H block");
        let (base, delta) = bc6h::layout::widths(mode);
        let (fields, partition) = bc6h::layout::fields(mode, block);
        let two_regions = mode <= 10;
        let mut s = [0u8; RECORD];
        s[30] = mode - 1;
        s[31] = partition;
        for c in 0..3 {
            let residual = |endpoint: usize, bits: u8| {
                let shift = 16 - u32::from(bits);
                // The field with its top bit flipped, against the top bits
                // of what the reference holds.
                let target = u32::from(fields[endpoint][c]) ^ 1 << (bits - 1);
                let from = u32::from(ends.ends[endpoint][c] >> shift);
                code(target.wrapping_sub(from), bits.into()) as u16
            };
            let first = residual(0, base);
            let second = residual(1, delta[c]);
            s[4 + 2 * c] = evens(first);
            s[5 + 2 * c] = evens(first >> 1);
            s[10 + 2 * c] = evens(second);
            s[11 + 2 * c] = evens(second >> 1);
            if two_regions {
                s[16 + 2 * c] = residual(2, delta[c]) as u8;
                s[17 + 2 * c] = residual(3, delta[c]) as u8;
            }
        }
        let bits: u8 = if two_regions { 3 } else { 4 };
        let anchor = if two_regions {
            usize::from(bc7::SECOND_OF_TWO[usize::from(partition)])
        } else {
            0
        };
        let mut reader = Reader(block >> bc6h::layout::header_bits(mode));
        let target = std::array::from_fn(|pixel| {
            let short = u32::from(pixel == 0 || pixel == anchor);
            // The block stores the index moved up by half its range.
            reader
                .take(u32::from(bits) - short)
                .wrapping_sub(1 << (bits - 1))
        });
        let from_bits = match picks.mode {
            1..=10 => 3,
            11..=14 => 4,
            _ => 0,
        };
        s[22..30].copy_from_slice(&pick_symbols(target, bits, &picks.picks, from_bits));
        s
    }
}

impl Unblock for Bc7<'_> {
    fn undo(&self, block: &[u8], ends: &bc7::State, picks: &bc7::State) -> [u8; RECORD] {
        let mut reader = Reader(u128::from_le_bytes(block.try_into().unwrap()));
        let mode = block[0].trailing_zeros();
        assert!(mode < 8, "not a BC7 block");
        reader.take(mode + 1);
        let (mut partition, mut rotation, mut swap_sets) = (0, 0, 0);
        match mode {
            0 => partition = reader.take(4),
            1 | 2 | 3 | 7 => partition = reader.take(6),
            4 => {
                rotation = reader.take(2);
                swap_sets = reader.take(1);
            }
            5 => rotation = reader.take(2),
            _ => {}
        }
        let subsets = u32::from(bc7::SUBSETS[mode as usize]);
        let word = (mode | rotation << 3 | swap_sets << 5 | subsets << 6 | partition << 8) as u16;
        let control = Control::of(word);
        let code_number = self
            .0
            .iter()
            .position(|&w| w == word)
            .expect("the control table lacks a word");
        let mut s = [0u8; RECORD];
        s[30] = (code_number & 31) as u8;
        s[31] = (code_number >> 5) as u8;

        // The fields, by end and channel, in the packed form's order.
        let widths = control.widths();
        let end_count = 2 * control.subsets;
        let mut fields = [[0u32; 4]; 6];
        let mut order = [0, 1, 2, 3];
        if control.rotation != 0 {
            order.swap(3, control.rotation - 1);
        }
        for channel in order {
            let width = u32::from(widths[channel]);
            if width == 0 {
                continue;
            }
            let drop = u32::from(channel == 0 && control.has_p_bits());
            for end in fields.iter_mut().take(end_count) {
                end[channel] = reader.take(width - drop) << drop;
            }
        }
        if mode == 1 {
            // The second endpoint's copy of the shared p-bit is never
            // stored, so it can be anything; here it is the same bit.
            for subset in 0..control.subsets {
                let bit = reader.take(1);
                fields[2 * subset][0] |= bit;
                fields[2 * subset + 1][0] |= bit;
            }
        } else if control.has_p_bits() {
            for end in fields.iter_mut().take(end_count) {
                end[0] |= reader.take(1);
            }
        }

        // Where the residual symbol of a subset's column travels.
        // The third subset's greens and blues ride in the alpha lanes.
        let place = |subset: usize, column: usize| match (subset, column) {
            (0, _) => 4 + column,
            (1, _) => 12 + column,
            (_, 0..=1) => 20 + column,
            (_, 2..=3) => 8 + column,
            (_, _) => 14 + column,
        };
        let shared = control.subsets.min(Control::of(ends.control).subsets);
        for subset in 0..control.subsets {
            for column in 0..8 {
                let width = u32::from(widths[column / 2]);
                if width == 0 {
                    continue;
                }
                let from = ends.ends[if subset < shared { subset } else { 0 }][column];
                let target = fields[2 * subset + column % 2][column / 2];
                let residual = target.wrapping_sub(u32::from(from >> (16 - width)));
                s[place(subset, column)] = code(residual, width) as u8;
            }
        }

        let anchors = control.anchors();
        let bits = control.pick_bits();
        let mut read_set = |set: usize| -> [u32; 16] {
            std::array::from_fn(|pixel| {
                let short = u32::from(anchors[..control.subsets].contains(&pixel));
                reader.take(u32::from(bits[set]) - short)
            })
        };
        let (first, second) = match mode {
            4 => {
                let second = read_set(1);
                (read_set(0), Some(second))
            }
            5 => (read_set(0), Some(read_set(1))),
            _ => (read_set(0), None),
        };
        let symbols = pick_symbols(first, bits[0], &picks.picks[0], picks.pick_bits[0]);
        s[22..30].copy_from_slice(&symbols);
        if let Some(second) = second {
            let symbols = pick_symbols(second, bits[1], &picks.picks[1], picks.pick_bits[1]);
            s[12..18].copy_from_slice(&symbols[..6]);
            s[20..22].copy_from_slice(&symbols[6..]);
        }
        s
    }
}

/// Splits a byte over the two symbols a braided lane gives it.
fn unbraid(value: u8) -> [u8; 2] {
    [evens(value.into()), evens(u16::from(value) >> 1)]
}

/// Turns blocks into records, with references picked at random: none, a
/// near block, or anywhere in reach, which early in the data means the
/// dictionary. Every record is checked against the decoder's `block` on the
/// spot.
pub(crate) fn records<F: Unblock>(
    format: &F,
    dictionary: &[F::State],
    blocks: &[u8],
    rng: &mut Rng,
) -> Vec<[u8; RECORD]> {
    assert_eq!(dictionary.len(), DICTIONARY);
    let none = F::State::default();
    let mut recent = vec![none; HISTORY];
    let mut records = Vec::new();
    let back = |rng: &mut Rng| match rng.below(4) {
        0 => 0,
        1 => 1 + rng.below(3),
        _ => rng.below(256),
    } as u8;
    for (index, block) in blocks.chunks_exact(F::BLOCK).enumerate() {
        let (ends_back, picks_back) = (back(rng), back(rng));
        let ends = referred(index, ends_back.into(), &recent, dictionary, &none);
        let picks = referred(index, picks_back.into(), &recent, dictionary, &none);
        let mut record = format.undo(block, ends, picks);
        let step = code(ends_back.wrapping_sub(picks_back).into(), 8) as u8;
        if F::PER == 2 {
            record[..2].copy_from_slice(&unbraid(step));
            record[2..4].copy_from_slice(&unbraid(picks_back));
        } else {
            record[0] = step;
            record[1] = picks_back;
        }
        for (at, &symbol) in record[..F::SYMBOL_BITS.len() * F::PER].iter().enumerate() {
            let bits = F::SYMBOL_BITS[at / F::PER];
            assert!(
                u32::from(symbol) < 1 << bits,
                "symbol {at} of block {index} does not fit its lane"
            );
        }
        let mut rebuilt = vec![0u8; F::BLOCK];
        let state = format
            .block(&record, ends, picks, &mut rebuilt)
            .expect("the record of a valid block is refused");
        assert_eq!(rebuilt, block, "block {index}");
        recent[index % HISTORY] = state;
        records.push(record);
    }
    records
}
