//! BC1, BC3, BC4 and BC5: blocks made of one or two simple halves.
//!
//! A *colour half* is what a BC1 block is: two 5:6:5 colours and sixteen
//! 2-bit selectors. A *ramp half* is what a BC4 block is: two 8-bit ends and
//! sixteen 3-bit selectors. BC3 is a ramp half (alpha) followed by a colour
//! half, BC5 is two ramp halves.
//!
//! The packed form keeps a selector as its *position on the line* between the
//! two ends (0 at the first end, the highest value at the second), not as the
//! code the block stores, so that neighbouring selectors differ by small
//! steps. Every element is the sum of a residual and the same element of the
//! referenced block, cut to the element's width.

use super::bits::step8;
use super::{Error, Format, RECORD};

/// The packed form of a colour half.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Colour {
    /// Red, green and blue of the first colour, then of the second.
    pub(super) ends: [u8; 6],
    /// Line positions of the sixteen pixels, 0 to 3.
    pub(super) picks: [u8; 16],
}

/// Bits of the six colour elements.
const COLOUR_BITS: [u8; 6] = [5, 6, 5, 5, 6, 5];

impl Colour {
    /// Builds a half from the residual symbols of its ends, the four symbols
    /// of its selectors, and the halves it refers to.
    ///
    /// One selector symbol stands for a 2x2 quad of pixels. `rank` turns it
    /// into the four 2-bit residuals: the table is ordered so that likely
    /// combinations get small symbols.
    fn build(
        ends: [u8; 6],
        quads: [u8; 4],
        rank: &[u8; 256],
        ends_from: &Colour,
        picks_from: &Colour,
    ) -> Colour {
        let mut half = Colour::default();
        for (k, bits) in COLOUR_BITS.into_iter().enumerate() {
            half.ends[k] = step8(ends[k]).wrapping_add(ends_from.ends[k]) & ((1 << bits) - 1);
        }
        for (quad, symbol) in quads.into_iter().enumerate() {
            let steps = rank[usize::from(symbol)];
            // Quads lie left to right, top to bottom; inside one, the pixels
            // do too.
            let corner = (quad >> 1) * 8 + (quad & 1) * 2;
            for (k, pixel) in [0, 1, 4, 5].into_iter().enumerate() {
                let at = corner + pixel;
                half.picks[at] = (steps >> (2 * k)).wrapping_add(picks_from.picks[at]) & 3;
            }
        }
        half
    }

    /// Writes the half as the eight bytes of a BC1 block.
    fn write(&self, out: &mut [u8]) {
        let e = self.ends.map(u16::from);
        let first = e[0] << 11 | e[1] << 5 | e[2];
        let second = e[3] << 11 | e[4] << 5 | e[5];
        let mut selectors = 0u32;
        for (pixel, &pick) in self.picks.iter().enumerate() {
            selectors |= u32::from(COLOUR_CODES[usize::from(pick & 3)]) << (2 * pixel);
        }
        out[..2].copy_from_slice(&first.to_le_bytes());
        out[2..4].copy_from_slice(&second.to_le_bytes());
        out[4..8].copy_from_slice(&selectors.to_le_bytes());
    }
}

/// The packed form of a ramp half.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Ramp {
    /// The two ends, each 128 less than the byte the block stores (modulo
    /// 256).
    pub(super) ends: [u8; 2],
    /// Line positions of the sixteen pixels, 0 to 7.
    pub(super) picks: [u8; 16],
}

impl Ramp {
    /// Builds a half from the residual symbols of its ends, the eight
    /// symbols of its selectors, and the halves it refers to.
    ///
    /// One selector symbol stands for two neighbouring pixels. `rank` turns
    /// it into their two 3-bit residuals, in bits 0-2 and 4-6.
    fn build(
        ends: [u8; 2],
        pairs: &[u8],
        rank: &[u8; 64],
        ends_from: &Ramp,
        picks_from: &Ramp,
    ) -> Ramp {
        let mut half = Ramp::default();
        for (k, symbol) in ends.into_iter().enumerate() {
            half.ends[k] = step8(symbol).wrapping_add(ends_from.ends[k]);
        }
        for (pair, &symbol) in pairs.iter().take(8).enumerate() {
            let steps = rank[usize::from(symbol & 63)];
            for (k, shift) in [0, 4].into_iter().enumerate() {
                let at = 2 * pair + k;
                half.picks[at] = (steps >> shift).wrapping_add(picks_from.picks[at]) & 7;
            }
        }
        half
    }

    /// Writes the half as the eight bytes of a BC4 block.
    fn write(&self, out: &mut [u8]) {
        let mut block = u64::from(self.ends[0] ^ 0x80) | u64::from(self.ends[1] ^ 0x80) << 8;
        for (pixel, &pick) in self.picks.iter().enumerate() {
            block |= u64::from(RAMP_CODES[usize::from(pick & 7)]) << (16 + 3 * pixel);
        }
        out[..8].copy_from_slice(&block.to_le_bytes());
    }
}

/// The selector code a block stores for each position on the line. Blocks
/// give codes 0 and 1 to the two ends and the codes from 2 up to the points
/// between them, in order.
const fn stored_codes<const N: usize>() -> [u8; N] {
    let mut codes = [0; N];
    let mut position = 1;
    while position < N {
        codes[position] = if position == N - 1 {
            1
        } else {
            position as u8 + 1
        };
        position += 1;
    }
    codes
}

const COLOUR_CODES: [u8; 4] = stored_codes();
const RAMP_CODES: [u8; 8] = stored_codes();

/// BC1: one colour half. Twelve lanes, one symbol each per block: the two
/// references, six end residuals, four selector quads.
pub(super) struct Bc1<'t>(pub(super) &'t [u8; 256]);

impl Format for Bc1<'_> {
    type State = Colour;
    const SYMBOL_BITS: &'static [u8] = &[8, 8, 5, 6, 5, 5, 6, 5, 8, 8, 8, 8];
    const PER: usize = 1;
    const BLOCK: usize = 8;

    fn block(
        &self,
        s: &[u8; RECORD],
        ends: &Colour,
        picks: &Colour,
        out: &mut [u8],
    ) -> Result<Colour, Error> {
        let half = Colour::build(
            [s[2], s[3], s[4], s[5], s[6], s[7]],
            [s[8], s[9], s[10], s[11]],
            self.0,
            ends,
            picks,
        );
        half.write(out);
        Ok(half)
    }
}

/// BC4: one ramp half. Twelve lanes, one symbol each per block: the two
/// references, two end residuals, eight selector pairs.
pub(super) struct Bc4<'t>(pub(super) &'t [u8; 64]);

impl Format for Bc4<'_> {
    type State = Ramp;
    const SYMBOL_BITS: &'static [u8] = &[8, 8, 8, 8, 6, 6, 6, 6, 6, 6, 6, 6];
    const PER: usize = 1;
    const BLOCK: usize = 8;

    fn block(
        &self,
        s: &[u8; RECORD],
        ends: &Ramp,
        picks: &Ramp,
        out: &mut [u8],
    ) -> Result<Ramp, Error> {
        let half = Ramp::build([s[2], s[3]], &s[4..12], self.0, ends, picks);
        half.write(out);
        Ok(half)
    }
}

/// BC3: a ramp half for alpha, then a colour half. Twelve lanes, two symbols
/// each per block: the references, the alpha ends, eight alpha selector
/// pairs, the two reds, the two greens, the two blues, four colour selector
/// quads.
pub(super) struct Bc3<'t> {
    pub(super) alpha: &'t [u8; 64],
    pub(super) colour: &'t [u8; 256],
}

impl Format for Bc3<'_> {
    type State = (Ramp, Colour);
    const SYMBOL_BITS: &'static [u8] = &[4, 4, 8, 6, 6, 6, 6, 5, 6, 5, 8, 8];
    const PER: usize = 2;
    const BLOCK: usize = 16;

    fn block(
        &self,
        s: &[u8; RECORD],
        ends: &Self::State,
        picks: &Self::State,
        out: &mut [u8],
    ) -> Result<Self::State, Error> {
        let alpha = Ramp::build([s[4], s[5]], &s[6..14], self.alpha, &ends.0, &picks.0);
        let colour = Colour::build(
            [s[14], s[16], s[18], s[15], s[17], s[19]],
            [s[20], s[21], s[22], s[23]],
            self.colour,
            &ends.1,
            &picks.1,
        );
        let (first, second) = out.split_at_mut(8);
        alpha.write(first);
        colour.write(second);
        Ok((alpha, colour))
    }
}

/// BC5: two ramp halves. Twelve lanes, two symbols each per block: the
/// references, the ends of the first half, the ends of the second, sixteen
/// selector pairs.
pub(super) struct Bc5<'t>(pub(super) &'t [u8; 64]);

impl Format for Bc5<'_> {
    type State = [Ramp; 2];
    const SYMBOL_BITS: &'static [u8] = &[4, 4, 8, 8, 6, 6, 6, 6, 6, 6, 6, 6];
    const PER: usize = 2;
    const BLOCK: usize = 16;

    fn block(
        &self,
        s: &[u8; RECORD],
        ends: &Self::State,
        picks: &Self::State,
        out: &mut [u8],
    ) -> Result<Self::State, Error> {
        let halves = [
            Ramp::build([s[4], s[5]], &s[8..16], self.0, &ends[0], &picks[0]),
            Ramp::build([s[6], s[7]], &s[16..24], self.0, &ends[1], &picks[1]),
        ];
        let (first, second) = out.split_at_mut(8);
        halves[0].write(first);
        halves[1].write(second);
        Ok(halves)
    }
}
