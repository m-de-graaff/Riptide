//! Tests of the BCPack decoder, on synthetic input only: no real tables file
//! and no title data.
//!
//! The tables are made up (`made`), and so are the streams: `pack` is a
//! packer for the entropy layer and `unblock` turns blocks back into
//! records, so whole blocks can go through the packed form and back.
//!
//! What this proves is that the decoder is the inverse of that packer, on
//! every format, with references into earlier blocks and into the
//! dictionary, with every BC6H and BC7 mode, and that damaged input is
//! refused or decoded to garbage without a panic. A few blocks are also
//! checked against bytes worked out by hand. What it cannot prove is that
//! the packer's idea of the format is the console's: packer and decoder were
//! written from the same description. That was checked outside the
//! repository, against another decoder and real tables.

pub(crate) mod made;
pub(crate) mod pack;
pub(crate) mod unblock;

use super::bc6h::{self, Bc6h};
use super::bc7::Bc7;
use super::lanes::{BUCKETS, Bytes, Lanes, MODELS, Model};
use super::plain::{Bc1, Bc3, Bc4, Bc5};
use super::{Error, Format, RECORD, TABLES_BYTES, Tables, decode};
use pack::Rng;
use unblock::Unblock;

/// The formats the decoder has.
const FORMATS: [u8; 6] = [1, 3, 4, 5, 6, 7];

fn block_bytes(format: u8) -> usize {
    if matches!(format, 1 | 4) { 8 } else { 16 }
}

/// Random blocks that are valid blocks of the format. Any bytes are a block
/// of BC1 to BC5; BC6H and BC7 need the bits of a mode at the start.
pub(crate) fn random_blocks(format: u8, count: usize, rng: &mut Rng) -> Vec<u8> {
    let mut blocks = rng.bytes(count * block_bytes(format));
    if matches!(format, 6 | 7) {
        for block in blocks.chunks_exact_mut(16) {
            let (code, bits) = if format == 7 {
                let mode = rng.below(8) as u8;
                (1 << mode, mode + 1)
            } else {
                bc6h::layout::code(1 + rng.below(14) as u8)
            };
            block[0] = block[0] & !(((1u16 << bits) - 1) as u8) | code;
        }
    }
    blocks
}

fn pack_format<F: Unblock>(
    format: &F,
    dictionary: &[F::State],
    tables: &Tables,
    blocks: &[u8],
    rng: &mut Rng,
) -> Vec<u8> {
    let records = unblock::records(format, dictionary, blocks, rng);
    pack::stream(tables, F::SYMBOL_BITS, F::PER, &records, rng)
}

/// Packs blocks of a format.
pub(crate) fn pack_blocks(format: u8, t: &Tables, blocks: &[u8], rng: &mut Rng) -> Vec<u8> {
    match format {
        1 => pack_format(&Bc1(&t.bc1_quads), &t.bc1, t, blocks, rng),
        3 => {
            let format = Bc3 {
                alpha: &t.bc3_pairs,
                colour: &t.bc3_quads,
            };
            pack_format(&format, &t.bc3, t, blocks, rng)
        }
        4 => pack_format(&Bc4(&t.bc4_pairs), &t.bc4, t, blocks, rng),
        5 => pack_format(&Bc5(&t.bc5_pairs), &t.bc5, t, blocks, rng),
        6 => pack_format(&Bc6h, &t.bc6h, t, blocks, rng),
        7 => pack_format(&Bc7(&t.bc7_controls), &t.bc7, t, blocks, rng),
        _ => panic!("no such format"),
    }
}

fn lanes_of(format: u8) -> (&'static [u8], usize) {
    match format {
        1 => (Bc1::SYMBOL_BITS, Bc1::PER),
        3 => (Bc3::SYMBOL_BITS, Bc3::PER),
        4 => (Bc4::SYMBOL_BITS, Bc4::PER),
        5 => (Bc5::SYMBOL_BITS, Bc5::PER),
        6 => (Bc6h::SYMBOL_BITS, Bc6h::PER),
        _ => (Bc7::SYMBOL_BITS, Bc7::PER),
    }
}

fn plain_tables() -> Tables {
    Tables::parse(&made::plain()).unwrap()
}

fn rough_tables(seed: u64) -> Tables {
    Tables::parse(&made::rough(seed)).unwrap()
}

#[test]
fn a_tables_file_has_one_length() {
    assert_eq!(made::plain().len(), TABLES_BYTES);
    assert!(Tables::parse(&made::plain()).is_ok());
    assert!(Tables::parse(&made::rough(1)).is_ok());
    let mut long = made::plain();
    long.push(0);
    assert_eq!(
        Tables::parse(&long).unwrap_err(),
        Error::TablesLength(TABLES_BYTES + 1)
    );
    assert_eq!(Tables::parse(&[]).unwrap_err(), Error::TablesLength(0));
}

#[test]
fn a_model_that_does_not_fill_its_slots_is_refused() {
    // One slot too many in the last bucket of model 37.
    let mut file = made::plain();
    let at = (37 * BUCKETS + 37 / 16 + 1) * 2;
    file[at] += 1;
    assert_eq!(Tables::parse(&file).unwrap_err(), Error::TablesModel(37));
    // An empty bucket in the first model.
    let mut file = made::plain();
    file[..2].fill(0);
    assert_eq!(Tables::parse(&file).unwrap_err(), Error::TablesModel(0));
    // All zeroes.
    assert_eq!(
        Tables::parse(&vec![0; TABLES_BYTES]).unwrap_err(),
        Error::TablesModel(0)
    );
}

#[test]
fn a_control_word_with_the_wrong_subsets_is_refused() {
    // Mode 6 has one subset; this word claims three.
    let mut file = made::plain();
    let word: u16 = 6 | 3 << 6;
    file[made::CONTROLS_AT + 2 * 300..][..2].copy_from_slice(&word.to_le_bytes());
    assert_eq!(Tables::parse(&file).unwrap_err(), Error::TablesControl(300));
}

/// Decoding a symbol the slow, obvious way: find the bucket by walking the
/// shares, and divide.
fn decode_plainly(shares: &[u16; BUCKETS], buckets: usize, state: u32) -> (u8, u32) {
    let slot = state & 0xffff;
    let mut first = 0u32;
    for (bucket, &share) in shares.iter().enumerate().take(buckets) {
        let share = u32::from(share);
        let symbols = 1u32 << bucket.saturating_sub(1);
        if slot < first + share * symbols {
            return if bucket == 0 {
                (
                    0,
                    (state >> 16)
                        .wrapping_mul(share - 1)
                        .wrapping_add(slot.wrapping_sub(1)),
                )
            } else {
                let nth = (slot - first) / share;
                let within = (slot - first) % share;
                ((symbols + nth) as u8, (state >> 16) * share + within)
            };
        }
        first += share * symbols;
    }
    unreachable!("the shares fill every slot")
}

#[test]
fn a_model_decodes_every_slot_like_plain_division() {
    for file in [made::plain(), made::rough(7), made::rough(8)] {
        for index in 0..MODELS {
            let shares: [u16; BUCKETS] = std::array::from_fn(|bucket| {
                let at = (index * BUCKETS + bucket) * 2;
                u16::from_le_bytes([file[at], file[at + 1]])
            });
            let buckets = index / 16 + 2;
            let model = Model::new(&shares, buckets).unwrap();
            for high in [0x0100u32, 0x5a5a, 0xffff] {
                for slot in 0..1u32 << 16 {
                    let state = high << 16 | slot;
                    assert_eq!(
                        model.decode(state),
                        decode_plainly(&shares, buckets, state),
                        "model {index}, state {state:#x}"
                    );
                }
            }
        }
    }
}

#[test]
fn every_symbol_of_a_model_has_the_slots_it_decodes_from() {
    let tables = rough_tables(3);
    for (index, model) in tables.models.iter().enumerate() {
        for symbol in 0..(2u32 << (index / 16)) {
            let (first, count) = model.slots(symbol as u8);
            for slot in [first, first + count - 1] {
                assert_eq!(model.decode(0x1234 << 16 | slot).0, symbol as u8);
            }
        }
    }
}

#[test]
fn symbols_come_back_through_the_lanes_of_every_format() {
    for (tables, seed) in [(plain_tables(), 11), (rough_tables(12), 13)] {
        let mut rng = Rng(seed);
        for format in FORMATS {
            let (symbol_bits, per) = lanes_of(format);
            for count in [1, 2, 9, 130, 2500] {
                let records = pack::random_records(symbol_bits, per, count, &mut rng);
                // `stream` checks the records against its own run of the
                // lanes; this is the run over the finished stream.
                let stream = pack::stream(&tables, symbol_bits, per, &records, &mut rng);
                let mut lanes = Lanes::new(&tables.models, symbol_bits, per, Bytes(stream.iter()));
                for (index, record) in records.iter().enumerate() {
                    let mut got = [0u8; RECORD];
                    lanes.record(&mut got).unwrap();
                    assert_eq!(&got, record, "BC{format}, record {index} of {count}");
                }
            }
        }
    }
}

#[test]
fn blocks_of_every_format_come_back() {
    for (tables, seed) in [
        (plain_tables(), 21),
        (rough_tables(22), 23),
        (rough_tables(24), 25),
    ] {
        let mut rng = Rng(seed);
        for format in FORMATS {
            // Past 256 blocks the references wrap around the history; before
            // that, many of them land in the dictionary.
            for count in [1, 3, 40, 700] {
                let blocks = random_blocks(format, count, &mut rng);
                let packed = pack_blocks(format, &tables, &blocks, &mut rng);
                let mut out = vec![0xa5; blocks.len()];
                decode(format, &packed, &mut out, &tables).unwrap();
                assert!(out == blocks, "BC{format}, {count} blocks");
            }
        }
    }
}

#[test]
fn every_bc7_and_bc6h_mode_comes_back() {
    let tables = rough_tables(31);
    let mut rng = Rng(32);
    for format in [6, 7] {
        let blocks = random_blocks(format, 4000, &mut rng);
        let mut seen = [false; 16];
        for block in blocks.chunks_exact(16) {
            let mode = if format == 7 {
                block[0].trailing_zeros() as usize
            } else {
                usize::from(
                    bc6h::layout::mode_of(u128::from_le_bytes(block.try_into().unwrap())).unwrap(),
                )
            };
            seen[mode] = true;
        }
        let expected = if format == 7 { 0..8 } else { 1..15 };
        assert!(expected.into_iter().all(|mode| seen[mode]));
        let packed = pack_blocks(format, &tables, &blocks, &mut rng);
        let mut out = vec![0; blocks.len()];
        decode(format, &packed, &mut out, &tables).unwrap();
        assert!(out == blocks);
    }
}

/// A record with the given symbols at the given places.
fn record(symbols: &[(usize, u8)]) -> [u8; RECORD] {
    let mut record = [0u8; RECORD];
    for &(at, symbol) in symbols {
        record[at] = symbol;
    }
    record
}

fn decode_records<F: Format>(tables: &Tables, format: u8, records: &[[u8; RECORD]]) -> Vec<u8> {
    let packed = pack::stream(tables, F::SYMBOL_BITS, F::PER, records, &mut Rng(41));
    let mut out = vec![0; records.len() * F::BLOCK];
    decode(format, &packed, &mut out, tables).unwrap();
    out
}

#[test]
fn bc1_block_worked_out_by_hand() {
    // Colours (31, 2, 0) and (0, 63, 3): residuals -1, 2, 0, 0, -1, 3 from
    // nothing. Every quad holds the line positions 0, 1, 2, 3, which blocks
    // store as the codes 0, 2, 3, 1.
    let ends = [(2, 1), (3, 4), (4, 0), (5, 0), (6, 1), (7, 6)];
    let quads = [(8, 0xe4), (9, 0xe4), (10, 0xe4), (11, 0xe4)];
    let out = decode_records::<Bc1>(
        &plain_tables(),
        1,
        &[record(&[&ends[..], &quads[..]].concat())],
    );
    assert_eq!(out, [0x40, 0xf8, 0xe3, 0x07, 0x88, 0x77, 0x88, 0x77]);
}

#[test]
fn bc4_blocks_worked_out_by_hand() {
    // Block 0: ends +7 and -5 (stored 128 higher), line positions 7 down to
    // 0 twice over, stored as the codes 1, 7, 6, 5, 4, 3, 2, 0.
    let mut first = vec![(2, 14), (3, 9)];
    for pair in 0..8u8 {
        let left = 7 - 2 * (pair % 4);
        first.push((4 + usize::from(pair), left | (left - 1) << 3));
    }
    // Block 1: indices continue block 0 (one back), each two further along
    // the line; the endpoints continue nothing (the endpoint reference is
    // the index reference minus one) and are zero.
    let mut second = vec![(0, 1), (1, 1)];
    second.extend((4..12).map(|at| (at, 2 | 2 << 3)));
    let out = decode_records::<Bc4>(&plain_tables(), 4, &[record(&first), record(&second)]);
    assert_eq!(out[..8], [0x87, 0x7b, 0xb9, 0xcb, 0x09, 0xb9, 0xcb, 0x09]);
    assert_eq!(out[8..], [0x80, 0x80, 0x42, 0xee, 0x72, 0x42, 0xee, 0x72]);
}

#[test]
fn bc7_mode_6_block_worked_out_by_hand() {
    // Mode 6: one subset, red of 7 bits and a p-bit kept as red's lowest
    // bit, green, blue and alpha of 7 bits, 4-bit indices (left at zero).
    let code = made::plain_code(6 | 1 << 6);
    let symbols = [
        (4, 6),    // red 0: +3, so field 1 and p-bit 1
        (5, 1),    // red 1: -1, so field 0x7f and p-bit 1
        (6, 10),   // green 0: 5
        (8, 3),    // blue 0: -2, so 0x7e
        (10, 254), // alpha 0: 127
        (11, 2),   // alpha 1: 1
        (30, (code & 31) as u8),
        (31, (code >> 5) as u8),
    ];
    let out = decode_records::<Bc7>(&plain_tables(), 7, &[record(&symbols)]);
    let expected: u128 = 0x40
        | 1 << 7
        | 0x7f << 14
        | 5 << 21
        | 0x7e << 35
        | 0x7f << 49
        | 1 << 56
        | 1 << 63
        | 1 << 64;
    assert_eq!(out, expected.to_le_bytes());
}

#[test]
fn a_reference_before_the_first_block_reads_the_dictionary() {
    // With a dictionary of noise, the first block built on entry 5 differs
    // from the same record built on nothing by exactly that entry.
    let tables = rough_tables(51);
    let format = Bc4(&tables.bc4_pairs);
    let symbols = record(&[(2, 3), (3, 8), (4, 1)]);
    let mut on_nothing = [0u8; 8];
    let none = Default::default();
    let bare = format
        .block(&symbols, &none, &none, &mut on_nothing)
        .unwrap();
    // Reference 5 from block 0 is five before the start: entry 4.
    let mut referring = symbols;
    referring[1] = 5;
    let out = decode_records::<Bc4>(&tables, 4, &[referring]);
    let entry = &tables.bc4[4];
    let mut expected = [0u8; 8];
    let built = format.block(&symbols, entry, entry, &mut expected).unwrap();
    assert_eq!(out, expected);
    assert_eq!(built.ends[0], bare.ends[0].wrapping_add(entry.ends[0]));
}

#[test]
fn formats_and_lengths_that_cannot_be_are_refused() {
    let tables = plain_tables();
    let mut out = [0u8; 32];
    assert_eq!(decode(2, &[], &mut out, &tables), Err(Error::Bc2));
    assert_eq!(
        decode(0, &[], &mut out, &tables),
        Err(Error::UnknownFormat(0))
    );
    assert_eq!(
        decode(8, &[], &mut out, &tables),
        Err(Error::UnknownFormat(8))
    );
    for format in FORMATS {
        let odd = block_bytes(format) + 4;
        assert_eq!(
            decode(format, &[], &mut out[..odd], &tables),
            Err(Error::OutputLength { format, bytes: odd })
        );
        // Nothing to decode is not an error; something from nothing is.
        assert_eq!(decode(format, &[], &mut [], &tables), Ok(()));
        assert_eq!(decode(format, &[], &mut out, &tables), Err(Error::Ended));
    }
}

#[test]
fn every_error_says_something() {
    let errors = [
        Error::UnknownFormat(9),
        Error::Bc2,
        Error::OutputLength {
            format: 7,
            bytes: 5,
        },
        Error::Ended,
        Error::Bc6hMode,
        Error::Bc7Control,
        Error::TablesLength(3),
        Error::TablesModel(4),
        Error::TablesControl(5),
    ];
    for error in errors {
        let boxed: Box<dyn std::error::Error> = Box::new(error);
        assert!(boxed.to_string().len() > 20);
    }
}

#[test]
fn cut_short_data_is_refused_or_already_complete() {
    let tables = rough_tables(61);
    let mut rng = Rng(62);
    for format in FORMATS {
        let blocks = random_blocks(format, 24, &mut rng);
        let packed = pack_blocks(format, &tables, &blocks, &mut rng);
        for length in 0..packed.len() {
            let mut out = vec![0; blocks.len()];
            // The last few bytes may belong to lanes that had already
            // finished; without them the blocks are still all there.
            match decode(format, &packed[..length], &mut out, &tables) {
                Ok(()) => assert!(out == blocks, "BC{format} cut to {length}"),
                Err(error) => assert_eq!(error, Error::Ended),
            }
        }
        let mut out = vec![0; blocks.len()];
        let half = &packed[..packed.len() / 2];
        assert_eq!(decode(format, half, &mut out, &tables), Err(Error::Ended));
        // More blocks asked for than were packed.
        let mut more = vec![0; blocks.len() + 64 * block_bytes(format)];
        assert_eq!(
            decode(format, &packed, &mut more, &tables),
            Err(Error::Ended)
        );
    }
}

#[test]
fn headers_that_promise_more_than_there_is_are_refused() {
    let tables = plain_tables();
    for format in FORMATS {
        for byte in [0x30, 0xff, 0x00] {
            let mut out = [0u8; 16];
            let result = decode(format, &[byte; 24], &mut out, &tables);
            assert_eq!(result, Err(Error::Ended), "BC{format}, bytes of {byte:#x}");
        }
    }
}

#[test]
fn an_unused_bc7_code_and_a_bc6h_mode_past_14_are_refused() {
    let tables = plain_tables();
    // Code 511 is past the 285 words `plain` defines.
    let unused = record(&[(30, 31), (31, 15)]);
    let packed = pack::stream(&tables, Bc7::SYMBOL_BITS, 2, &[unused], &mut Rng(71));
    assert_eq!(
        decode(7, &packed, &mut [0; 16], &tables),
        Err(Error::Bc7Control)
    );
    // The mode symbol is the mode less one: 14 would be mode 15.
    for symbol in [14, 31] {
        let record = record(&[(30, symbol)]);
        let packed = pack::stream(&tables, Bc6h::SYMBOL_BITS, 2, &[record], &mut Rng(72));
        assert_eq!(
            decode(6, &packed, &mut [0; 16], &tables),
            Err(Error::Bc6hMode)
        );
    }
}

#[test]
fn noise_never_panics() {
    for tables in [plain_tables(), rough_tables(81)] {
        let mut rng = Rng(82);
        for round in 0..400 {
            let noise = rng.bytes(round * 5 % 1300);
            for format in 0..=8 {
                let mut out = vec![0; block_bytes(format) * (1 + round % 50)];
                let _ = decode(format, &noise, &mut out, &tables);
            }
        }
    }
}

#[test]
fn damaged_data_never_panics() {
    let tables = rough_tables(91);
    let mut rng = Rng(92);
    for format in FORMATS {
        let blocks = random_blocks(format, 120, &mut rng);
        let packed = pack_blocks(format, &tables, &blocks, &mut rng);
        for _ in 0..300 {
            let mut damaged = packed.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(damaged.len() as u64) as usize;
                damaged[at] ^= 1 << rng.below(8);
            }
            let mut out = vec![0; blocks.len()];
            let _ = decode(format, &damaged, &mut out, &tables);
        }
    }
}

/// Decoding speed on made-up streams of random blocks, which are the worst
/// case: nothing in them is likely, so every block costs as many packed
/// bytes as it can.
/// `cargo test --release -p riptide-formats bcpack_speed -- --ignored --nocapture`
#[test]
#[ignore = "a measurement, not a check"]
fn bcpack_speed() {
    let tables = rough_tables(101);
    let mut rng = Rng(102);
    for format in FORMATS {
        let blocks = random_blocks(format, 1 << 14, &mut rng);
        let packed = pack_blocks(format, &tables, &blocks, &mut rng);
        let mut out = vec![0; blocks.len()];
        let runs = 30;
        let started = std::time::Instant::now();
        for _ in 0..runs {
            decode(format, std::hint::black_box(&packed), &mut out, &tables).unwrap();
        }
        let seconds = started.elapsed().as_secs_f64() / f64::from(runs);
        println!(
            "BC{format}: {} packed bytes to {} bytes, {:.1} MB out per second",
            packed.len(),
            out.len(),
            out.len() as f64 / seconds / 1e6
        );
    }
}
