//! The entropy layer: lanes of range-coded symbols and the schedule that
//! deals the packed bytes out to them.
//!
//! The module documentation of [`super`] describes the layer. What matters
//! here is that the schedule is defined by what each lane does step by step,
//! so this file is a small machine that is run exactly as the console's
//! hardware runs it: one round at a time, every lane one step per round.

use super::Error;

/// How many models the format has: sixteen for each alphabet size.
pub(super) const MODELS: usize = 128;
/// Models of one alphabet size.
const SHAPES: usize = 16;
/// The most buckets a model has (the 256-symbol alphabet).
pub(super) const BUCKETS: usize = 9;
/// A model divides this many slots between its symbols.
pub(super) const SLOTS: u32 = 1 << 16;
/// A lane reads a byte while its state is below this, and decodes a symbol
/// while it is above.
pub(super) const FLOOR: u32 = 1 << 24;
/// Bytes the lanes may take between them in one round.
const ROUND_BYTES: u32 = 8;
/// The most lanes a format has.
pub(super) const MAX_LANES: usize = 16;
/// Rounds in which nothing is read and nothing delivered before the data is
/// given up as ended or damaged.
const QUIET_ROUNDS: u32 = 3;

/// One static model: how the 65536 slots are divided between the symbols of
/// an alphabet.
///
/// Symbols are grouped in buckets. Bucket 0 is symbol 0 alone; bucket `b`
/// holds the `2^(b-1)` symbols from `2^(b-1)` up, and all symbols of a
/// bucket have the same share. A model for `2^n` symbols has `n + 1`
/// buckets.
#[derive(Clone)]
pub(super) struct Model {
    /// The first slot of each bucket; 65536 for buckets the model lacks.
    first: [u32; BUCKETS],
    /// The slots of one symbol of each bucket.
    share: [u32; BUCKETS],
    /// `ceil(2^32 / share)`: multiplying by it and dropping 32 bits divides
    /// exactly, because both operands are below 65536.
    inverse: [u64; BUCKETS],
}

impl Model {
    /// Builds a model of `buckets` buckets from the share of a symbol of
    /// each. Returns nothing if a bucket has no share or if the shares do not
    /// add up to exactly 65536 slots, since the decoder relies on both.
    pub(super) fn new(shares: &[u16; BUCKETS], buckets: usize) -> Option<Model> {
        let mut model = Model {
            first: [SLOTS; BUCKETS],
            share: [0; BUCKETS],
            inverse: [0; BUCKETS],
        };
        let mut next = 0u32;
        for (bucket, &share) in shares.iter().enumerate().take(buckets) {
            let share = u32::from(share);
            if share == 0 {
                return None;
            }
            model.first[bucket] = next;
            model.share[bucket] = share;
            model.inverse[bucket] = (1u64 << 32).div_ceil(u64::from(share));
            next += share << bucket.saturating_sub(1);
        }
        (next == SLOTS).then_some(model)
    }

    /// Decodes one symbol from a lane's state and returns it with the state
    /// that remains.
    ///
    /// The low sixteen bits of the state name a slot, and the slot belongs to
    /// one symbol. Symbol 0 is special: slot 0 is never produced by a packer,
    /// so symbol 0 owns one slot fewer than its share and its slots are
    /// counted from 1.
    #[inline]
    pub(super) fn decode(&self, state: u32) -> (u8, u32) {
        let slot = state & (SLOTS - 1);
        let bucket = self.first[1..].iter().filter(|&&f| f <= slot).count();
        let (symbol, share, within) = if bucket == 0 {
            (0, self.share[0] - 1, slot.wrapping_sub(1))
        } else {
            let offset = slot - self.first[bucket];
            let nth = ((u64::from(offset) * self.inverse[bucket]) >> 32) as u32;
            (
                (1u32 << (bucket - 1)) + nth,
                self.share[bucket],
                offset - nth * self.share[bucket],
            )
        };
        (
            symbol as u8,
            (state >> 16).wrapping_mul(share).wrapping_add(within),
        )
    }

    /// The slots a packer may use for `symbol` (first slot and how many), for
    /// the test packer.
    #[cfg(test)]
    pub(super) fn slots(&self, symbol: u8) -> (u32, u32) {
        if symbol == 0 {
            return (1, self.share[0] - 1);
        }
        let bucket = 8 - symbol.leading_zeros() as usize;
        let nth = u32::from(symbol) - (1 << (bucket - 1));
        (
            self.first[bucket] + nth * self.share[bucket],
            self.share[bucket],
        )
    }
}

/// The index of the model for an alphabet of `2^(group + 1)` symbols and one
/// of its sixteen shapes.
pub(super) fn model_index(group: u8, shape: u8) -> usize {
    usize::from(group) * SHAPES + usize::from(shape) % SHAPES
}

/// Where the packed bytes come from. The decoder reads them in order from a
/// slice; the test packer answers per lane, which is how it learns the order
/// the bytes must be written in.
pub(super) trait Source {
    /// The next byte, which `lane` will consume. `None` when there is none.
    fn next(&mut self, lane: usize) -> Option<u8>;
}

/// The packed data of a request, read front to back.
pub(super) struct Bytes<'a>(pub(super) std::slice::Iter<'a, u8>);

impl Source for Bytes<'_> {
    #[inline]
    fn next(&mut self, _lane: usize) -> Option<u8> {
        self.0.next().copied()
    }
}

/// What a lane expects its next byte to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// A fragment's header byte.
    Header,
    /// The length byte (the low one of a long fragment).
    Length,
    /// The high length byte of a long fragment.
    LongLength,
    /// Payload: range-coded symbols.
    Payload,
}

#[derive(Clone, Copy)]
struct Lane {
    /// The alphabet group of this lane's widest model: symbol bits less one.
    group: u8,
    stage: Stage,
    /// The fragment's length takes two bytes.
    long: bool,
    /// The state starts from zero again after this fragment.
    restart: bool,
    model: u8,
    state: u32,
    /// Payload bytes of the fragment not yet shifted into the state.
    left: u32,
    /// A byte taken from the input and not consumed yet. A lane holds at
    /// most one.
    input: Option<u8>,
    /// A decoded symbol not yet placed in the record.
    symbol: Option<u8>,
    /// The lane wants a byte.
    hungry: bool,
    /// The lane sits out its next step: it wanted a byte the round could not
    /// give, or its symbol has to wait for the other lanes.
    held: bool,
}

impl Lane {
    fn new(symbol_bits: u8) -> Lane {
        Lane {
            group: symbol_bits.clamp(1, 8) - 1,
            stage: Stage::Header,
            long: false,
            restart: false,
            model: 0,
            state: 0,
            left: 0,
            input: None,
            symbol: None,
            hungry: true,
            held: false,
        }
    }

    /// One step: consume the byte in hand if the lane has a use for it, and
    /// decode at most one symbol.
    #[inline]
    fn step(&mut self, models: &[Model]) {
        if self.stage == Stage::Payload {
            return self.payload(models);
        }
        let Some(byte) = self.input.take() else {
            return;
        };
        self.hungry = true;
        match self.stage {
            Stage::Header => {
                // Bits 6-7 halve the alphabet that many times, bits 0-3 pick
                // the shape, bit 4 restarts the state afterwards, bit 5 means
                // a two-byte length.
                let group = self.group.saturating_sub(byte >> 6);
                self.model = model_index(group, byte & 15) as u8;
                self.restart = byte & 0x10 != 0;
                self.long = byte & 0x20 != 0;
                self.stage = Stage::Length;
            }
            Stage::Length if self.long => {
                self.left = u32::from(byte);
                self.stage = Stage::LongLength;
            }
            Stage::Length => {
                self.left = u32::from(byte) + 1;
                self.stage = Stage::Payload;
            }
            Stage::LongLength | Stage::Payload => {
                self.left += (u32::from(byte) << 8) + 257;
                self.stage = Stage::Payload;
            }
        }
    }

    #[inline]
    fn payload(&mut self, models: &[Model]) {
        let mut state = self.state;
        if state < FLOOR
            && let Some(byte) = self.input.take()
        {
            self.left = self.left.saturating_sub(1);
            state = state << 8 | u32::from(byte);
        }
        self.hungry = self.left != 0 && self.input.is_none();
        if state > FLOOR {
            let Some(model) = models.get(usize::from(self.model)) else {
                return;
            };
            let (symbol, rest) = model.decode(state);
            self.state = rest;
            self.symbol = Some(symbol);
        } else if self.left != 0 {
            // More bytes to come. A state of exactly the floor neither reads
            // nor decodes, so a lane that reaches it here is parked for good.
            self.state = state;
        } else {
            // The fragment is over. The state carries into the next fragment
            // unless the header said to restart; a byte shifted in during
            // this very step is not part of what carries.
            self.stage = Stage::Header;
            self.hungry = self.input.is_none();
            if self.restart {
                self.state = 0;
            }
        }
    }
}

/// The lanes of one packed stream, delivering one record at a time.
pub(super) struct Lanes<'t, S> {
    models: &'t [Model],
    source: S,
    lanes: [Lane; MAX_LANES],
    count: usize,
    /// Symbols each lane gives to one record.
    per: usize,
    /// One bit per lane that has placed its symbol for the current position.
    placed: u32,
    quiet: u32,
}

impl<'t, S: Source> Lanes<'t, S> {
    /// Sets up one lane per entry of `symbol_bits` (the size of each lane's
    /// full alphabet, in bits), each giving `per` symbols to every record.
    pub(super) fn new(models: &'t [Model], symbol_bits: &[u8], per: usize, source: S) -> Self {
        let count = symbol_bits.len().min(MAX_LANES);
        let mut lanes = [Lane::new(8); MAX_LANES];
        for (lane, &bits) in lanes.iter_mut().zip(symbol_bits) {
            *lane = Lane::new(bits);
        }
        Lanes {
            models,
            source,
            lanes,
            count,
            per: per.clamp(1, 2),
            placed: 0,
            quiet: 0,
        }
    }

    /// Decodes the next record: `per` symbols of lane 0, then of lane 1, and
    /// so on.
    ///
    /// Each round every lane takes one step, in lane order, and may then take
    /// one byte from the input, as long as the lanes have taken fewer than
    /// eight between them this round. A lane whose symbol is ready places it
    /// in the record unless it has already placed one for this position;
    /// then it waits, without stepping, until all lanes have placed theirs
    /// and the position moves on.
    pub(super) fn record(&mut self, record: &mut [u8; super::RECORD]) -> Result<(), Error> {
        let everyone = (1u32 << self.count) - 1;
        let mut position = 0;
        while position < self.per {
            let mut taken = 0;
            let mut moved = false;
            for (index, lane) in self.lanes[..self.count].iter_mut().enumerate() {
                let bit = 1 << index;
                let spent = taken >= ROUND_BYTES;
                if !lane.held {
                    lane.step(self.models);
                }
                let waits = lane.symbol.is_some() && self.placed & bit != 0;
                if lane.hungry
                    && !waits
                    && !spent
                    && let Some(byte) = self.source.next(index)
                {
                    lane.input = Some(byte);
                    taken += 1;
                }
                lane.held = (lane.hungry && spent) || waits;
                if !lane.held
                    && let Some(symbol) = lane.symbol.take()
                {
                    record[index * self.per + position] = symbol;
                    self.placed |= bit;
                    moved = true;
                }
            }
            if self.placed == everyone {
                self.placed = 0;
                position += 1;
                moved = true;
            }
            if taken > 0 || moved {
                self.quiet = 0;
            } else {
                self.quiet += 1;
                if self.quiet > QUIET_ROUNDS {
                    return Err(Error::Ended);
                }
            }
        }
        Ok(())
    }

    /// Gives the source back, for the test packer.
    #[cfg(test)]
    pub(super) fn into_source(self) -> S {
        self.source
    }
}
