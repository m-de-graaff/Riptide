//! Small pieces every format shares: the bit writer for a 16-byte block, the
//! signed codes of residuals, and the way two symbols are braided into one
//! value.

/// Writes fields into a block of up to 128 bits, lowest bit first, which is
/// the order of every block-compressed format.
#[derive(Default)]
pub(super) struct BlockBits {
    bits: u128,
    used: u32,
}

impl BlockBits {
    /// Appends the low `count` bits of `value`. `count` is at most 64. Bits
    /// past the 128th are dropped: no valid block has any.
    pub(super) fn put(&mut self, value: u64, count: u32) {
        if count == 0 {
            return;
        }
        if self.used < 128 {
            let kept = u128::from(value) & ((1u128 << count.min(64)) - 1);
            self.bits |= kept << self.used;
        }
        self.used = self.used.saturating_add(count);
    }

    /// The block as it lies in memory.
    pub(super) fn bytes(&self) -> [u8; 16] {
        self.bits.to_le_bytes()
    }
}

/// A residual symbol to a signed step, as a byte that wraps: 0, 1, 2, 3, 4
/// mean 0, -1, 1, -2, 2. The caller cuts the result to the element's width.
pub(super) fn step8(symbol: u8) -> u8 {
    (symbol >> 1) ^ (symbol & 1).wrapping_neg()
}

/// The same code on 16 bits.
pub(super) fn step16(symbol: u16) -> u16 {
    (symbol >> 1) ^ (symbol & 1).wrapping_neg()
}

/// Moves the eight bits of `value` to the even positions of sixteen.
fn spread(value: u8) -> u16 {
    let mut v = u16::from(value);
    v = (v | v << 4) & 0x0f0f;
    v = (v | v << 2) & 0x3333;
    (v | v << 1) & 0x5555
}

/// Two symbols as one value: `even` on the even bits, `odd` on the odd bits.
/// Wide values are spread over a lane's two symbols this way, so that both
/// symbols stay small when the value is small.
pub(super) fn braid(even: u8, odd: u8) -> u16 {
    spread(even) | spread(odd) << 1
}

/// The bits at the even positions of a byte, as four bits.
fn evens(value: u8) -> u8 {
    let v = value & 0x55;
    let v = (v | v >> 1) & 0x33;
    (v | v >> 2) & 0x0f
}

/// The index residuals of one block of BC6H or of one index set of BC7:
/// eight symbols, two pixels each, in raster order.
///
/// A symbol braids two four-bit signed codes. In the first three rows of the
/// block the code on the odd bits belongs to the left pixel of the pair; in
/// the last row it is the other way round.
pub(super) fn pick_steps(symbols: &[u8; 8]) -> [i16; 16] {
    let signed = |code: u8| i16::from(code >> 1) ^ -i16::from(code & 1);
    let mut steps = [0i16; 16];
    for (pair, &symbol) in symbols.iter().enumerate() {
        let (even, odd) = (signed(evens(symbol)), signed(evens(symbol >> 1)));
        let (left, right) = if pair < 6 { (odd, even) } else { (even, odd) };
        steps[2 * pair] = left;
        steps[2 * pair + 1] = right;
    }
    steps
}

/// An index of `bits` bits from its residual and the index it refers to,
/// which may have a different width (`from_bits`, 0 when there is none).
///
/// Both are aligned at the top of sixteen bits before they are added, so a
/// reference index of another width contributes its most significant bits.
/// The result is sign-extended from `bits` bits; a block only stores its low
/// `bits` bits.
pub(super) fn rebase(step: i16, bits: u8, from: i16, from_bits: u8) -> i16 {
    if bits == 0 || bits > 16 {
        return 0;
    }
    let top = |value: i16, width: u8| match width {
        1..=16 => (value as u16) << (16 - width),
        _ => 0,
    };
    (top(step, bits).wrapping_add(top(from, from_bits)) as i16) >> (16 - bits)
}

/// What a record's first symbols say about the blocks it builds on.
pub(super) struct Refers {
    /// How far back the block is whose endpoints this one continues; 0 for
    /// none.
    pub(super) ends: usize,
    /// The same for the indices.
    pub(super) picks: usize,
}

impl Refers {
    /// Reads the references of a record. The first value is the endpoint
    /// reference as a signed step from the index reference, the second the
    /// index reference. Formats whose lanes carry two symbols per block
    /// (`braided`) hold each as two four-bit symbols braided together.
    pub(super) fn of(record: &[u8; super::RECORD], braided: bool) -> Refers {
        let (step, picks) = if braided {
            (
                braid(record[0] & 15, record[1] & 15) as u8,
                braid(record[2] & 15, record[3] & 15) as u8,
            )
        } else {
            (record[0], record[1])
        };
        Refers {
            ends: usize::from(picks.wrapping_add(step8(step))),
            picks: usize::from(picks),
        }
    }
}
