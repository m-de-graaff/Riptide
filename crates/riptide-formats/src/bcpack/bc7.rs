//! BC7 blocks.
//!
//! A BC7 block has one of eight modes. A mode fixes the number of subsets
//! (one to three, each with two endpoints), the bits of each channel, whether
//! endpoints carry an extra shared low bit (the *p-bit*), and the width of
//! the indices; modes 4 and 5 have two sets of indices and can swap alpha
//! with a colour channel (the *rotation*). All of that is Microsoft's public
//! description of the format, as are the tables in this file.
//!
//! The packed form does not spend symbols on the mode bits, the partition
//! number and the rotation one by one. Two symbols together make a code, and
//! a table of the format turns the code into a *control word* that holds
//! them all. Endpoint fields are kept aligned at the top of sixteen bits,
//! like in BC6H, so that a block of one mode can continue a block of another.

use super::bits::{BlockBits, pick_steps, rebase, step16};
use super::{Error, Format, RECORD};

/// How many control codes there are.
pub(super) const CODES: usize = 512;

/// The pixel that, in a block of two subsets, stores the index of the second
/// subset one bit short. By partition number.
#[rustfmt::skip]
pub(super) const SECOND_OF_TWO: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15,
    15, 15, 15, 15, 15, 15, 15, 15,
    15,  2,  8,  2,  2,  8,  8, 15,
     2,  8,  2,  2,  8,  8,  2,  2,
    15, 15,  6,  8,  2,  8, 15, 15,
     2,  8,  2,  2,  2, 15, 15,  6,
     6,  2,  6,  8, 15, 15,  2,  2,
    15, 15, 15, 15, 15,  2,  2, 15,
];

/// The same for the second subset of three.
#[rustfmt::skip]
const SECOND_OF_THREE: [u8; 64] = [
     3,  3, 15, 15,  8,  3, 15, 15,
     8,  8,  6,  6,  6,  5,  3,  3,
     3,  3,  8, 15,  3,  3,  6, 10,
     5,  8,  8,  6,  8,  5, 15, 15,
     8, 15,  3,  5,  6, 10,  8, 15,
    15,  3, 15,  5, 15, 15, 15, 15,
     3, 15,  5,  5,  5,  8,  5, 10,
     5, 10,  8, 13, 15, 12,  3,  3,
];

/// The same for the third subset of three.
#[rustfmt::skip]
const THIRD_OF_THREE: [u8; 64] = [
    15,  8,  8,  3, 15, 15,  3,  8,
    15, 15, 15, 15, 15, 15, 15,  8,
    15,  8, 15,  3, 15,  8, 15,  8,
     3, 15,  6, 10, 15, 15, 10,  8,
    15,  3, 15, 10, 10,  8,  9, 10,
     6, 15,  8, 15,  3,  6,  6,  8,
    15,  3, 15, 15, 15, 15, 15, 15,
    15, 15, 15, 15,  3, 15, 15,  8,
];

/// Subsets of each mode.
pub(super) const SUBSETS: [u8; 8] = [3, 2, 3, 2, 1, 1, 1, 2];
/// Bits of a colour channel of each mode, without the p-bit.
const COLOUR_BITS: [u8; 8] = [4, 6, 5, 7, 5, 7, 7, 5];
/// Bits of alpha of each mode, without the p-bit.
const ALPHA_BITS: [u8; 8] = [0, 0, 0, 0, 6, 8, 7, 5];
/// Bits of an index of the first and the second set of each mode.
const PICK_BITS: [[u8; 2]; 8] = [
    [3, 0],
    [3, 0],
    [2, 0],
    [2, 0],
    [3, 2],
    [2, 2],
    [4, 0],
    [2, 0],
];

/// A control word taken apart.
#[derive(Clone, Copy)]
pub(super) struct Control {
    /// Bits 0-2.
    pub(super) mode: usize,
    /// Bits 3-4: 0, or one more than the channel that changed places with
    /// alpha. Only modes 4 and 5 have a rotation; the bits mean nothing in
    /// the others.
    pub(super) rotation: usize,
    /// Bit 5: the bit of mode 4 that says which index set the colours use.
    pub(super) swap_sets: bool,
    /// Bits 6-7.
    pub(super) subsets: usize,
    /// Bits 8-13.
    pub(super) partition: usize,
}

impl Control {
    pub(super) fn of(word: u16) -> Control {
        let mode = usize::from(word & 7);
        Control {
            mode,
            rotation: if matches!(mode, 4 | 5) {
                usize::from(word >> 3 & 3)
            } else {
                0
            },
            swap_sets: word & 0x20 != 0,
            subsets: usize::from(word >> 6 & 3),
            partition: usize::from(word >> 8 & 63),
        }
    }

    /// Whether the endpoints of the mode carry p-bits.
    pub(super) fn has_p_bits(&self) -> bool {
        matches!(self.mode, 0 | 1 | 3 | 6 | 7)
    }

    /// Bits of the red, green, blue and alpha the packed form keeps.
    ///
    /// The packed form keeps the channels in the order the block stores
    /// them, so with a rotation the widths of alpha and of the rotated
    /// channel change places. A p-bit is kept as one more low bit of red.
    pub(super) fn widths(&self) -> [u8; 4] {
        let colour = COLOUR_BITS[self.mode];
        let mut widths = [colour, colour, colour, ALPHA_BITS[self.mode]];
        if self.rotation != 0 {
            widths.swap(3, self.rotation - 1);
        } else if self.has_p_bits() {
            widths[0] += 1;
        }
        widths
    }

    /// Bits of an index of the first and the second set.
    pub(super) fn pick_bits(&self) -> [u8; 2] {
        PICK_BITS[self.mode]
    }

    /// The pixels whose indices are stored one bit short: the first of each
    /// subset.
    pub(super) fn anchors(&self) -> [usize; 3] {
        let second = match self.subsets {
            2 => SECOND_OF_TWO[self.partition],
            3 => SECOND_OF_THREE[self.partition],
            _ => 0,
        };
        let third = match self.subsets {
            3 => THIRD_OF_THREE[self.partition],
            _ => 0,
        };
        [0, second.into(), third.into()]
    }
}

/// What a later block can refer to.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct State {
    /// The block's control word; 0 where there is no block.
    pub(super) control: u16,
    /// Per subset: the two reds, the two greens, the two blues, the two
    /// alphas, top-aligned.
    pub(super) ends: [[u16; 8]; 3],
    /// Bits of an index of each set.
    pub(super) pick_bits: [u8; 2],
    /// The indices of each set, sign-extended from their width.
    pub(super) picks: [[i16; 16]; 2],
}

/// BC7. Sixteen lanes, two symbols each per block. Lanes 0 and 1 hold the
/// references and lane 15 the control code. Lanes 2 to 10 hold endpoint
/// residuals, and what each one holds depends on the mode (see `block`).
/// Lanes 11 to 14 hold the residuals of the first index set.
pub(super) struct Bc7<'t>(pub(super) &'t [u16; CODES]);

impl Format for Bc7<'_> {
    type State = State;
    const SYMBOL_BITS: &'static [u8] = &[4, 4, 8, 8, 8, 8, 8, 7, 7, 5, 6, 8, 8, 8, 8, 5];
    const PER: usize = 2;
    const BLOCK: usize = 16;

    fn block(
        &self,
        s: &[u8; RECORD],
        ends: &State,
        picks: &State,
        out: &mut [u8],
    ) -> Result<State, Error> {
        let word = self.0[usize::from(s[31] & 15) << 5 | usize::from(s[30] & 31)];
        if word == 0 {
            return Err(Error::Bc7Control);
        }
        let control = Control::of(word);
        let widths = control.widths();
        let mut state = State {
            control: word,
            pick_bits: control.pick_bits(),
            ..State::default()
        };

        // Residual symbols by subset and column. Lanes 2-4 are the colours
        // of the first subset, lanes 6-8 of the second, lane 10 the reds of
        // the third. Lanes 5 and 9 are the alphas of the first two subsets,
        // or, in a block of three subsets (which has no alpha), the greens
        // and blues of the third.
        let mut symbols = [[0u8; 8]; 3];
        symbols[0][..6].copy_from_slice(&s[4..10]);
        symbols[1][..6].copy_from_slice(&s[12..18]);
        symbols[2][..2].copy_from_slice(&s[20..22]);
        if control.subsets == 3 {
            symbols[2][2..4].copy_from_slice(&s[10..12]);
            symbols[2][4..6].copy_from_slice(&s[18..20]);
        } else {
            symbols[0][6..8].copy_from_slice(&s[10..12]);
            symbols[1][6..8].copy_from_slice(&s[18..20]);
        }
        // A subset continues the same subset of the referenced block. If
        // that block has fewer subsets, the others continue its first.
        let shared = control.subsets.min(Control::of(ends.control).subsets);
        for column in 0..8 {
            let width = u32::from(widths[column / 2]);
            if width == 0 {
                continue;
            }
            let shift = 16 - width;
            for (subset, symbols) in symbols.iter().enumerate().take(control.subsets) {
                let from = ends.ends[if subset < shared { subset } else { 0 }][column];
                let step = step16(symbols[column].into());
                state.ends[subset][column] = ((from >> shift) << shift).wrapping_add(step << shift);
            }
        }

        // The second index set exists in modes 4 and 5, which have one
        // subset; its symbols travel in the lanes of the subsets the block
        // does not have.
        let sets = [
            [s[22], s[23], s[24], s[25], s[26], s[27], s[28], s[29]],
            [s[12], s[13], s[14], s[15], s[16], s[17], s[20], s[21]],
        ];
        for (set, symbols) in sets.iter().enumerate() {
            let bits = state.pick_bits[set];
            if bits == 0 {
                continue;
            }
            for (pixel, step) in pick_steps(symbols).into_iter().enumerate() {
                state.picks[set][pixel] =
                    rebase(step, bits, picks.picks[set][pixel], picks.pick_bits[set]);
            }
        }

        out.copy_from_slice(&write(&control, &state, widths));
        Ok(state)
    }
}

/// Lays the fields out as the BC7 block of the control word's mode.
fn write(control: &Control, state: &State, widths: [u8; 4]) -> [u8; 16] {
    let mode = control.mode;
    let mut block = BlockBits::default();
    block.put(1 << mode, mode as u32 + 1);
    match mode {
        0 => block.put(control.partition as u64, 4),
        1 | 2 | 3 | 7 => block.put(control.partition as u64, 6),
        4 => {
            block.put(control.rotation as u64, 2);
            block.put(control.swap_sets.into(), 1);
        }
        5 => block.put(control.rotation as u64, 2),
        _ => {}
    }
    let field = |end: usize, channel: usize| {
        let width = u32::from(widths[channel]);
        u64::from(state.ends[end / 2][2 * channel + end % 2] >> (16 - width))
    };
    let ends = 2 * control.subsets;
    // The block stores the channels in the packed form's order, except that
    // a rotation moved alpha's field to the place of a colour.
    let mut order = [0, 1, 2, 3];
    if control.rotation != 0 {
        order.swap(3, control.rotation - 1);
    }
    let p_bits = control.has_p_bits();
    for channel in order {
        let width = u32::from(widths[channel]);
        if width == 0 {
            continue;
        }
        let drop = u32::from(channel == 0 && p_bits);
        for end in 0..ends {
            block.put(field(end, channel) >> drop, width - drop);
        }
    }
    if mode == 1 {
        // One p-bit for both endpoints of a subset.
        for subset in 0..control.subsets {
            block.put(field(2 * subset, 0) & 1, 1);
        }
    } else if p_bits {
        for end in 0..ends {
            block.put(field(end, 0) & 1, 1);
        }
    }
    let anchors = control.anchors();
    let mut put_set = |set: usize| {
        let bits = u32::from(state.pick_bits[set]);
        for (pixel, &pick) in state.picks[set].iter().enumerate() {
            let short = u32::from(anchors[..control.subsets.max(1)].contains(&pixel));
            block.put(u64::from(pick as u16), bits.saturating_sub(short));
        }
    };
    match mode {
        4 => {
            put_set(1);
            put_set(0);
        }
        5 => {
            put_set(0);
            put_set(1);
        }
        _ => put_set(0),
    }
    block.bytes()
}
