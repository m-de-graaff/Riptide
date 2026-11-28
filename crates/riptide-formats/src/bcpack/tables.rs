//! The constants of the format, read from a file the user supplies.

use super::lanes::{BUCKETS, MODELS, Model};
use super::plain::{Colour, Ramp};
use super::{DICTIONARY, Error, bc6h, bc7};

/// Bytes of one dictionary entry of each format, in file order: BC1, BC3,
/// BC4, BC5, BC6H, BC7.
const ENTRY_BYTES: [usize; 6] = [6 + 16, 8 + 32, 2 + 16, 4 + 32, 25 + 33, 50 + 66];

/// Bytes of the five rank tables together.
const RANK_BYTES: usize = 256 + 64 + 256 + 64 + 64;

/// How long a tables file is, in bytes.
pub const TABLES_BYTES: usize = MODELS * BUCKETS * 2
    + RANK_BYTES
    + bc7::CODES * 2
    + DICTIONARY
        * (ENTRY_BYTES[0]
            + ENTRY_BYTES[1]
            + ENTRY_BYTES[2]
            + ENTRY_BYTES[3]
            + ENTRY_BYTES[4]
            + ENTRY_BYTES[5]);

/// The constants of the BCPack format: the entropy models, the tables that
/// order index combinations by likelihood, the BC7 control words, and the
/// dictionary of each format.
///
/// They are numbers the console's hardware has built in. They cannot be
/// derived, and they are not part of Riptide: the user supplies them as a
/// file, and [`Tables::parse`] reads it.
pub struct Tables {
    pub(super) models: Vec<Model>,
    /// BC1: a quad symbol to four 2-bit residuals.
    pub(super) bc1_quads: [u8; 256],
    /// BC3 alpha: a pair symbol to two 3-bit residuals.
    pub(super) bc3_pairs: [u8; 64],
    /// BC3 colour: a quad symbol to four 2-bit residuals.
    pub(super) bc3_quads: [u8; 256],
    pub(super) bc4_pairs: [u8; 64],
    pub(super) bc5_pairs: [u8; 64],
    pub(super) bc7_controls: [u16; bc7::CODES],
    pub(super) bc1: Vec<Colour>,
    pub(super) bc3: Vec<(Ramp, Colour)>,
    pub(super) bc4: Vec<Ramp>,
    pub(super) bc5: Vec<[Ramp; 2]>,
    pub(super) bc6h: Vec<bc6h::State>,
    pub(super) bc7: Vec<bc7::State>,
}

impl std::fmt::Debug for Tables {
    /// Says only what it is: the contents are not Riptide's to print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tables")
    }
}

/// Walks the file front to back. Reading past the end yields zeroes, which
/// cannot happen once the length has been checked.
struct File<'a>(&'a [u8]);

impl File<'_> {
    fn byte(&mut self) -> u8 {
        match self.0.split_first() {
            Some((&byte, rest)) => {
                self.0 = rest;
                byte
            }
            None => 0,
        }
    }

    fn word(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        std::array::from_fn(|_| self.byte())
    }

    fn colour(&mut self) -> Colour {
        Colour {
            ends: self.bytes(),
            picks: self.bytes(),
        }
    }

    fn ramp_ends(&mut self) -> [u8; 2] {
        self.bytes()
    }
}

impl Tables {
    /// Reads a tables file.
    ///
    /// The file is [`TABLES_BYTES`] long, little-endian, without padding:
    ///
    /// 1. For each of the 128 models, nine 16-bit numbers: the slots of one
    ///    symbol of each bucket. The model for an alphabet of `2^n` symbols
    ///    uses the first `n + 1`; models come in groups of sixteen, from the
    ///    2-symbol alphabet to the 256-symbol one.
    /// 2. The rank tables, one byte per symbol: BC1 quads (256), BC3 alpha
    ///    pairs (64), BC3 colour quads (256), BC4 pairs (64), BC5 pairs (64).
    /// 3. 512 16-bit BC7 control words; 0 marks a code that is not used.
    /// 4. The dictionaries, 255 entries each, nearest entry first. BC1: six
    ///    ends and sixteen selectors, a byte each. BC3: the six colour ends,
    ///    the two alpha ends, sixteen alpha selectors, sixteen colour
    ///    selectors. BC4: two ends, sixteen selectors. BC5: the ends of both
    ///    halves, then the selectors of both. BC6H: a byte that is not used,
    ///    twelve 16-bit endpoint values, the mode byte, sixteen 16-bit
    ///    indices. BC7: the control word, twenty-four 16-bit endpoint values,
    ///    the two index widths, then 64 bytes of indices (see below).
    ///
    /// A model whose shares do not add up to exactly 65536 slots, or a
    /// control word whose subset count is not that of its mode, is refused:
    /// the decoder relies on both.
    pub fn parse(bytes: &[u8]) -> Result<Tables, Error> {
        if bytes.len() != TABLES_BYTES {
            return Err(Error::TablesLength(bytes.len()));
        }
        let mut file = File(bytes);

        let mut models = Vec::with_capacity(MODELS);
        for index in 0..MODELS {
            let shares: [u16; BUCKETS] = std::array::from_fn(|_| file.word());
            // Sixteen models per alphabet; the smallest alphabet has two
            // symbols, so two buckets.
            let buckets = index / 16 + 2;
            models.push(Model::new(&shares, buckets).ok_or(Error::TablesModel(index))?);
        }

        let bc1_quads = file.bytes();
        let bc3_pairs = file.bytes();
        let bc3_quads = file.bytes();
        let bc4_pairs = file.bytes();
        let bc5_pairs = file.bytes();

        let bc7_controls: [u16; bc7::CODES] = std::array::from_fn(|_| file.word());
        for (code, &word) in bc7_controls.iter().enumerate() {
            let control = bc7::Control::of(word);
            if word != 0 && control.subsets != usize::from(bc7::SUBSETS[control.mode]) {
                return Err(Error::TablesControl(code));
            }
        }

        let bc1 = (0..DICTIONARY).map(|_| file.colour()).collect();
        let bc3 = (0..DICTIONARY)
            .map(|_| {
                let colour_ends = file.bytes();
                let alpha = Ramp {
                    ends: file.ramp_ends(),
                    picks: file.bytes(),
                };
                let colour = Colour {
                    ends: colour_ends,
                    picks: file.bytes(),
                };
                (alpha, colour)
            })
            .collect();
        let bc4 = (0..DICTIONARY)
            .map(|_| Ramp {
                ends: file.ramp_ends(),
                picks: file.bytes(),
            })
            .collect();
        let bc5 = (0..DICTIONARY)
            .map(|_| {
                let ends = [file.ramp_ends(), file.ramp_ends()];
                ends.map(|ends| Ramp {
                    ends,
                    picks: file.bytes(),
                })
            })
            .collect();
        let bc6h = (0..DICTIONARY)
            .map(|_| {
                file.byte();
                let mut entry = bc6h::State::default();
                for value in entry.ends.as_flattened_mut() {
                    *value = file.word();
                }
                entry.mode = file.byte();
                for pick in &mut entry.picks {
                    *pick = file.word() as i16;
                }
                entry
            })
            .collect();
        let bc7 = (0..DICTIONARY)
            .map(|nth| {
                let mut entry = bc7::State {
                    control: file.word(),
                    ..bc7::State::default()
                };
                for value in entry.ends.as_flattened_mut() {
                    *value = file.word();
                }
                entry.pick_bits = file.bytes();
                // The 64 bytes of indices are 32 numbers in the first entry
                // only. In every other entry the first two bytes repeat the
                // widths, and 31 numbers follow: the sixteen of the first
                // set and fifteen of the second, whose last stays 0.
                let count = if nth == 0 {
                    32
                } else {
                    file.word();
                    31
                };
                for pick in &mut entry.picks.as_flattened_mut()[..count] {
                    *pick = file.word() as i16;
                }
                entry
            })
            .collect();

        Ok(Tables {
            models,
            bc1_quads,
            bc3_pairs,
            bc3_quads,
            bc4_pairs,
            bc5_pairs,
            bc7_controls,
            bc1,
            bc3,
            bc4,
            bc5,
            bc6h,
            bc7,
        })
    }
}
