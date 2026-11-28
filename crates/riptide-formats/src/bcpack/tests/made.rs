//! Tables files made up for the tests. They have the layout of a real one
//! and none of its contents.

use super::super::TABLES_BYTES;
use super::super::bc7::SUBSETS;
use super::super::lanes::{BUCKETS, MODELS};
use super::pack::Rng;

/// Where the BC7 control words start in a tables file.
pub(crate) const CONTROLS_AT: usize = MODELS * BUCKETS * 2 + 256 + 64 + 256 + 64 + 64;

/// Every control word a BC7 block can need: each mode with each of its
/// partitions, rotations and index choices.
pub(crate) fn control_words() -> Vec<u16> {
    let mut words = Vec::new();
    for mode in 0..8u16 {
        let base = mode | u16::from(SUBSETS[usize::from(mode)]) << 6;
        match mode {
            0 => words.extend((0..16).map(|partition| base | partition << 8)),
            1 | 2 | 3 | 7 => words.extend((0..64).map(|partition| base | partition << 8)),
            4 => words.extend((0..8).map(|choice| base | choice << 3)),
            5 => words.extend((0..4).map(|rotation| base | rotation << 3)),
            _ => words.push(base),
        }
    }
    words
}

/// The code `plain` gives a control word.
pub(crate) fn plain_code(word: u16) -> usize {
    control_words().iter().position(|&w| w == word).unwrap()
}

/// The simplest tables: every symbol of an alphabet equally likely, rank
/// tables that leave a symbol's bits where they are, control codes in the
/// order of `control_words`, and dictionaries of zeroes.
pub(crate) fn plain() -> Vec<u8> {
    let mut file = Vec::new();
    for model in 0..MODELS {
        let group = model / 16;
        for bucket in 0..BUCKETS {
            let share: u16 = if bucket < group + 2 {
                1 << (15 - group)
            } else {
                0
            };
            file.extend_from_slice(&share.to_le_bytes());
        }
    }
    let quads = 0..=255u8;
    let pairs = (0..64u8).map(|v| (v & 7) | (v >> 3) << 4);
    file.extend(quads.clone());
    file.extend(pairs.clone());
    file.extend(quads);
    file.extend(pairs.clone());
    file.extend(pairs);
    let mut controls = control_words();
    controls.resize(512, 0);
    for word in controls {
        file.extend_from_slice(&word.to_le_bytes());
    }
    file.resize(TABLES_BYTES, 0);
    file
}

/// Tables with everything uneven: skewed models, shuffled rank tables,
/// control codes scattered over the 512, and dictionaries of noise.
pub(crate) fn rough(seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut file = Vec::new();
    for model in 0..MODELS {
        let buckets = model / 16 + 2;
        let mut shares = [0u16; BUCKETS];
        let mut used = 0u32;
        for (bucket, share) in shares.iter_mut().enumerate().take(buckets).skip(1) {
            // Every fourth shape leaves almost everything to symbol 0.
            let room = if model % 4 == 0 {
                3
            } else {
                (65534 / (buckets as u32 - 1)) >> (bucket - 1)
            };
            *share = 1 + rng.below(room.into()) as u16;
            used += u32::from(*share) << (bucket - 1);
        }
        shares[0] = (65536 - used) as u16;
        for share in shares {
            file.extend_from_slice(&share.to_le_bytes());
        }
    }
    let mut shuffled = |count: usize| {
        let mut order: Vec<u8> = (0..count).map(|v| v as u8).collect();
        for at in (1..count).rev() {
            order.swap(at, rng.below(at as u64 + 1) as usize);
        }
        order
    };
    let as_pairs = |order: Vec<u8>| order.into_iter().map(|v| (v & 7) | (v >> 3) << 4);
    file.extend(shuffled(256));
    file.extend(as_pairs(shuffled(64)));
    file.extend(shuffled(256));
    file.extend(as_pairs(shuffled(64)));
    file.extend(as_pairs(shuffled(64)));
    let mut controls = [0u16; 512];
    for (nth, word) in control_words().into_iter().enumerate() {
        controls[(nth * 37 + 5) % 512] = word;
    }
    for word in controls {
        file.extend_from_slice(&word.to_le_bytes());
    }
    let rest = TABLES_BYTES - file.len();
    file.extend(rng.bytes(rest));
    file
}
