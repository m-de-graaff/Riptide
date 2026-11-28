//! A packer for the entropy layer, for tests only.
//!
//! It range-codes the symbols of each lane into fragments, choosing models,
//! fragment lengths and restarts at random, and then finds the order the
//! bytes must be written in by running the decoder's own lanes against one
//! queue per lane. That is the only definition of the order there is.

use std::collections::VecDeque;

use super::super::lanes::{FLOOR, Lanes, Model, Source, model_index};
use super::super::{RECORD, Tables};

/// A small deterministic generator, so that failures repeat.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `limit`.
    pub(crate) fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }

    pub(crate) fn bytes(&mut self, count: usize) -> Vec<u8> {
        (0..count).map(|_| self.next() as u8).collect()
    }
}

/// One fragment being built.
struct Piece {
    symbols: std::ops::Range<usize>,
    /// How many times the header halves the lane's alphabet.
    shrink: u8,
    shape: u8,
    /// The state starts from zero after this fragment.
    restart: bool,
    /// The payload, last byte first: a range coder is written backwards.
    reversed: Vec<u8>,
}

/// Cuts a lane's symbols into fragments and picks a model for each.
fn pieces(models: &[Model], symbol_bits: u8, symbols: &[u8], rng: &mut Rng) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut at = 0;
    while at < symbols.len() {
        let length = match rng.below(8) {
            0 => 1 + rng.below(4),
            // Long enough that the payload passes 256 bytes and the length
            // needs two bytes.
            1 => 400 + rng.below(1500),
            _ => 1 + rng.below(200),
        } as usize;
        let range = at..symbols.len().min(at + length);
        at = range.end;
        let largest = symbols[range.clone()].iter().copied().max().unwrap_or(0);
        // The smallest alphabet that holds the symbols, as a group number:
        // group g is the alphabet of 2^(g+1) symbols.
        let needed = (8 - largest.leading_zeros()).max(1) as u8 - 1;
        let lane_group = symbol_bits - 1;
        let shrink = rng.below(u64::from((lane_group - needed).min(3)) + 1) as u8;
        // Any shape will do, as long as it gives every symbol a slot.
        let first_try = rng.below(16) as u8;
        let shape = (0..16)
            .map(|k| (first_try + k) % 16)
            .find(|&shape| {
                let model = &models[model_index(lane_group - shrink, shape)];
                symbols[range.clone()].iter().all(|&s| model.slots(s).1 > 0)
            })
            .expect("no model of the group can code these symbols");
        pieces.push(Piece {
            symbols: range,
            shrink,
            shape,
            restart: false,
            reversed: Vec::new(),
        });
    }
    pieces
}

/// The bytes of one lane: its fragments, headers and lengths included.
pub(crate) fn lane(models: &[Model], symbol_bits: u8, symbols: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut pieces = pieces(models, symbol_bits, symbols, rng);
    let count = pieces.len();
    // Backwards: the decoder ends where the packer starts, at the floor.
    let mut state = FLOOR;
    for index in (0..count).rev() {
        let model =
            &models[model_index(symbol_bits - 1 - pieces[index].shrink, pieces[index].shape)];
        let range = pieces[index].symbols.clone();
        let last = range.end - 1;
        for at in range.rev() {
            let (first, slots) = model.slots(symbols[at]);
            let full = u64::from(slots) << 16;
            // Where the bytes go that the decoder reads right after this
            // symbol. After the last symbol of a fragment they open the next
            // fragment, if the state carries over. It can only carry over if
            // there is at least one such byte: otherwise the decoder would
            // go on decoding with this fragment's model.
            let mut target = index;
            if at == last && index + 1 < count {
                if u64::from(state) >= full && rng.below(4) != 0 {
                    target = index + 1;
                } else {
                    pieces[index + 1]
                        .reversed
                        .extend_from_slice(&state.to_le_bytes());
                    pieces[index].restart = true;
                    state = FLOOR;
                }
            }
            while u64::from(state) >= full {
                pieces[target].reversed.push(state as u8);
                state >>= 8;
            }
            state = (state / slots) << 16 | (state % slots + first);
        }
    }
    if let Some(first) = pieces.first_mut() {
        first.reversed.extend_from_slice(&state.to_le_bytes());
    }

    let mut bytes = Vec::new();
    for piece in &pieces {
        let length = piece.reversed.len();
        assert!(
            (1..=65792).contains(&length),
            "a fragment of {length} bytes"
        );
        let long = length > 256;
        bytes.push(
            piece.shrink << 6 | u8::from(long) << 5 | u8::from(piece.restart) << 4 | piece.shape,
        );
        if long {
            bytes.extend_from_slice(&((length - 257) as u16).to_le_bytes());
        } else {
            bytes.push((length - 1) as u8);
        }
        bytes.extend(piece.reversed.iter().rev());
    }
    bytes
}

/// One queue of bytes per lane; remembers the order they were asked for.
struct Queues {
    lanes: Vec<VecDeque<u8>>,
    stream: Vec<u8>,
}

impl Source for Queues {
    fn next(&mut self, lane: usize) -> Option<u8> {
        // A lane that has given its last symbol may still ask for bytes
        // while the others finish. It gets filler: a fragment of 0xff bytes,
        // whose first symbol can never be placed, so the lane stops asking.
        let byte = self.lanes[lane].pop_front().unwrap_or(0xff);
        self.stream.push(byte);
        Some(byte)
    }
}

/// Packs records into a stream: the entropy layer of a format whose lanes
/// have the alphabets `symbol_bits` and give `per` symbols to each record.
pub(crate) fn stream(
    tables: &Tables,
    symbol_bits: &[u8],
    per: usize,
    records: &[[u8; RECORD]],
    rng: &mut Rng,
) -> Vec<u8> {
    let lanes = symbol_bits
        .iter()
        .enumerate()
        .map(|(index, &bits)| {
            let symbols: Vec<u8> = records
                .iter()
                .flat_map(|record| record[index * per..][..per].iter().copied())
                .collect();
            lane(&tables.models, bits, &symbols, rng).into()
        })
        .collect();
    let queues = Queues {
        lanes,
        stream: Vec::new(),
    };
    let mut decoder = Lanes::new(&tables.models, symbol_bits, per, queues);
    let used = symbol_bits.len() * per;
    for (index, record) in records.iter().enumerate() {
        let mut got = [0u8; RECORD];
        decoder
            .record(&mut got)
            .expect("the packer's own lanes stall");
        assert_eq!(got[..used], record[..used], "record {index}");
    }
    let queues = decoder.into_source();
    // The one or two bytes that bring a lane's state back to the floor after
    // its last symbol may stay unread: nothing waits for them.
    for (index, left) in queues.lanes.iter().enumerate() {
        assert!(left.len() <= 2, "lane {index} did not read all its bytes");
    }
    queues.stream
}

/// Records of random symbols that fit the lanes' alphabets.
pub(crate) fn random_records(
    symbol_bits: &[u8],
    per: usize,
    count: usize,
    rng: &mut Rng,
) -> Vec<[u8; RECORD]> {
    (0..count)
        .map(|_| {
            let mut record = [0u8; RECORD];
            for (at, symbol) in record[..symbol_bits.len() * per].iter_mut().enumerate() {
                let bits = symbol_bits[at / per];
                // Mostly small symbols, as in real data, so that the
                // smaller alphabets get used too.
                let bits = if rng.below(3) == 0 {
                    bits
                } else {
                    1 + rng.below(bits.into()) as u8
                };
                *symbol = rng.below(1 << bits) as u8;
            }
            record
        })
        .collect()
}
