//! BCPack: the console's lossless packing of block-compressed textures.
//!
//! Block-compressed textures (BC1 to BC7) are already compressed for the
//! GPU, but their blocks still repeat and resemble each other. BCPack is the
//! second, lossless compression the console applies on disc, and its storage
//! hardware undoes it while reading: a title asks DirectStorage for a read
//! "with BCPack, format BC7" and receives ordinary BC7 blocks. This module is
//! that unpacking, as a pure function with no state, no dependencies and no
//! unsafe code.
//!
//! # The format
//!
//! Packed data has no header and no length. It describes blocks one after
//! another, and the reader has to know the format and how many blocks to
//! expect. There are two layers.
//!
//! ## Records
//!
//! Each block is described by a **record** of small numbers, the *symbols*:
//! 12 for BC1 and BC4, 24 for BC3 and BC5, 32 for BC6H and BC7. A record
//! says three things.
//!
//! * Two **references**: which earlier block this block's endpoints resemble
//!   and which earlier block its indices resemble. A reference is a distance
//!   of 1 to 255 blocks back, or 0 for none. The two are chosen separately,
//!   and the first is stored as its difference from the second.
//! * The **endpoint residuals**: what to add to the referenced block's
//!   endpoints to get this block's.
//! * The **index residuals**: the same for the per-pixel indices. In the
//!   simple formats several pixels share one symbol, and a table of the
//!   format (a *rank table*) turns the symbol into their residuals.
//!
//! BC6H and BC7 records also name the block's mode and partition. With no
//! reference, the residuals are the values themselves.
//!
//! A reference that reaches back before the first block of the data does not
//! fail. It lands in the format's **dictionary**: 255 fixed blocks that
//! stand in for the blocks at positions -1 to -255, so that even the start of
//! a texture has something typical to build on.
//!
//! ## Lanes
//!
//! The symbols are entropy-coded, and not as one stream. Symbol `k` of every
//! record belongs to its own stream, a **lane**, because symbols with the
//! same meaning have similar statistics. (In the formats with 24 or 32
//! symbols a lane carries two neighbouring symbols of each record, so there
//! are 12 or 16 lanes.)
//!
//! A lane is a sequence of **fragments**. A fragment is a header byte, a
//! length of one or two bytes, and that many bytes of payload. The header
//! picks one of the format's 128 static **models** for the payload: the
//! frequency table of a range coder (rANS, with a 32-bit state fed a byte at
//! a time). It also says whether the coder's state carries over into the
//! next fragment or starts afresh. A packer starts a new fragment whenever
//! the statistics of a lane change enough for another model to pay.
//!
//! The lanes' bytes are interleaved in one stream, and nothing in the stream
//! says which byte belongs to which lane. A fixed **schedule** decides it:
//! the lanes take turns in rounds, each doing one small step of work per
//! round and asking for a byte when it has run dry, with at most eight bytes
//! handed out per round; a lane that is ahead of the others waits for them.
//! The hardware decodes all lanes at once this way. For a reader it means
//! the only way to find a lane's bytes is to run all lanes exactly as the
//! hardware does, and that is what `lanes.rs` does.
//!
//! # What is built and what is not
//!
//! BC1, BC3, BC4, BC5, BC6H and BC7 are decoded. BC2 is refused with
//! [`Error::Bc2`]: no description of its packed form is known, and the
//! decoder this one was checked against does not have it either.
//!
//! There is no packer. The tests contain one, enough to write streams this
//! decoder reads back.
//!
//! The constants of the format (the models, the rank tables, the BC7 control
//! words and the dictionaries) are not in this crate. They are loaded from a
//! file the user supplies; see [`Tables`].
//!
//! # Where the facts come from
//!
//! The format was learnt from the BCPack decoder of the VIctory project
//! (GPL-3.0-or-later), which is the only description of it available. The
//! block layouts of BC1 to BC7 themselves are Microsoft's public ones.
//! Nothing here was checked against the console. Agreement with that
//! decoder was checked outside the repository; README.md says how.
//!
//! # Damaged input
//!
//! The format has almost no redundancy. Truncated data is noticed, because
//! the lanes run dry, and so are a BC6H mode or a BC7 control code that does
//! not exist. Most other damage decodes to wrong blocks without complaint.
//! What is guaranteed is that no input makes the decoder panic, loop without
//! end, or read or write outside the slices it was given.

mod bc6h;
mod bc7;
mod bits;
mod lanes;
mod plain;
mod tables;

#[cfg(test)]
mod tests;

pub use tables::{TABLES_BYTES, Tables};

use bits::Refers;
use lanes::{Bytes, Lanes};

/// How many blocks back a reference can reach.
const HISTORY: usize = 256;
/// Entries of a format's dictionary.
const DICTIONARY: usize = 255;
/// Symbols of the longest record.
const RECORD: usize = 32;

/// Why data could not be unpacked, or tables could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Error {
    /// The format number is not one of 1 to 7.
    UnknownFormat(u8),
    /// The format is BC2, whose packed form is not built.
    Bc2,
    /// The output is not a whole number of blocks of the format. Holds the
    /// format and the length given.
    OutputLength { format: u8, bytes: usize },
    /// The packed data ran out, or stopped making progress, before every
    /// block was decoded.
    Ended,
    /// A BC6H record names a mode that does not exist.
    Bc6hMode,
    /// A BC7 record uses a control code the tables mark as unused.
    Bc7Control,
    /// The tables file does not have the length of a tables file. Holds the
    /// length given.
    TablesLength(usize),
    /// A model of the tables file has an empty bucket or does not divide
    /// exactly 65536 slots. Holds the model's number.
    TablesModel(usize),
    /// A BC7 control word of the tables file has a subset count its mode
    /// cannot have. Holds the code.
    TablesControl(usize),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Error::UnknownFormat(format) => {
                write!(
                    f,
                    "BCPack format {format} does not exist (1 to 7 mean BC1 to BC7)"
                )
            }
            Error::Bc2 => f.write_str("BCPack data of BC2 textures cannot be unpacked yet"),
            Error::OutputLength { format, bytes } => write!(
                f,
                "{bytes} bytes is not a whole number of BC{format} blocks"
            ),
            Error::Ended => {
                f.write_str("the BCPack data ends before all blocks are decoded, or is damaged")
            }
            Error::Bc6hMode => f.write_str("the BCPack data names a BC6H mode that does not exist"),
            Error::Bc7Control => {
                f.write_str("the BCPack data uses a BC7 control code that is not in use")
            }
            Error::TablesLength(bytes) => write!(
                f,
                "a BCPack tables file is {TABLES_BYTES} bytes long, not {bytes}"
            ),
            Error::TablesModel(model) => write!(
                f,
                "model {model} of the BCPack tables file does not divide 65536 slots"
            ),
            Error::TablesControl(code) => write!(
                f,
                "control code {code} of the BCPack tables file has a subset count its mode cannot have"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// One block-compressed format: its lanes, and how a record becomes a block.
trait Format {
    /// What is kept of a block for later blocks to refer to: its endpoints
    /// and indices in the packed form's own representation.
    type State: Copy + Default;
    /// The size of each lane's full alphabet, in bits.
    const SYMBOL_BITS: &'static [u8];
    /// Symbols a lane gives to one record: 1 or 2.
    const PER: usize;
    /// Bytes of a block.
    const BLOCK: usize;

    /// Builds the block of a record on the blocks it refers to, writes it to
    /// `out` (which is `BLOCK` bytes long) and returns its state.
    fn block(
        &self,
        record: &[u8; RECORD],
        ends: &Self::State,
        picks: &Self::State,
        out: &mut [u8],
    ) -> Result<Self::State, Error>;
}

/// The block a reference names: `back` blocks before block `index`. Before
/// the first block lies the dictionary, nearest entry first. `none` answers
/// a reference of 0.
fn referred<'a, T>(
    index: usize,
    back: usize,
    recent: &'a [T],
    dictionary: &'a [T],
    none: &'a T,
) -> &'a T {
    if back == 0 {
        none
    } else if back <= index {
        recent.get((index - back) % HISTORY).unwrap_or(none)
    } else {
        dictionary.get(back - index - 1).unwrap_or(none)
    }
}

fn unpack<F: Format>(
    format: &F,
    dictionary: &[F::State],
    tables: &Tables,
    packed: &[u8],
    blocks: &mut [u8],
) -> Result<(), Error> {
    let mut lanes = Lanes::new(&tables.models, F::SYMBOL_BITS, F::PER, Bytes(packed.iter()));
    let none = F::State::default();
    let mut recent = vec![none; HISTORY];
    let mut record = [0u8; RECORD];
    for (index, block) in blocks.chunks_exact_mut(F::BLOCK).enumerate() {
        lanes.record(&mut record)?;
        let refers = Refers::of(&record, F::PER == 2);
        let ends = referred(index, refers.ends, &recent, dictionary, &none);
        let picks = referred(index, refers.picks, &recent, dictionary, &none);
        let state = format.block(&record, ends, picks, block)?;
        recent[index % HISTORY] = state;
    }
    Ok(())
}

/// Unpacks BCPack data into blocks.
///
/// `format` is the number DirectStorage gives: 1 to 7 for BC1 to BC7. BC1,
/// BC3, BC4, BC5, BC6H (6) and BC7 are decoded; BC2 is refused with
/// [`Error::Bc2`].
///
/// `blocks` is the output and must be exactly as long as the decoded data: a
/// whole number of blocks of 8 bytes (BC1, BC4) or 16 bytes (the others).
/// Its length is what says how many blocks to decode, because the packed
/// data does not. Bytes of `packed` after the last one the blocks need are
/// ignored. An empty output succeeds without reading anything.
///
/// On an error `blocks` holds the blocks decoded so far and is otherwise
/// untouched. No input causes a panic or an access outside the two slices;
/// the work is bounded by their lengths.
pub fn decode(format: u8, packed: &[u8], blocks: &mut [u8], tables: &Tables) -> Result<(), Error> {
    let block = match format {
        1 | 4 => 8,
        3 | 5 | 6 | 7 => 16,
        2 => return Err(Error::Bc2),
        _ => return Err(Error::UnknownFormat(format)),
    };
    if !blocks.len().is_multiple_of(block) {
        return Err(Error::OutputLength {
            format,
            bytes: blocks.len(),
        });
    }
    let t = tables;
    match format {
        1 => unpack(&plain::Bc1(&t.bc1_quads), &t.bc1, t, packed, blocks),
        3 => {
            let format = plain::Bc3 {
                alpha: &t.bc3_pairs,
                colour: &t.bc3_quads,
            };
            unpack(&format, &t.bc3, t, packed, blocks)
        }
        4 => unpack(&plain::Bc4(&t.bc4_pairs), &t.bc4, t, packed, blocks),
        5 => unpack(&plain::Bc5(&t.bc5_pairs), &t.bc5, t, packed, blocks),
        6 => unpack(&bc6h::Bc6h, &t.bc6h, t, packed, blocks),
        _ => unpack(&bc7::Bc7(&t.bc7_controls), &t.bc7, t, packed, blocks),
    }
}
