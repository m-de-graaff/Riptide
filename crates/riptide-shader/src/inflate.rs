//! Unpacking zlib streams.
//!
//! The console compiler stores a shader's machine code as a zlib stream (RFC
//! 1950) around DEFLATE data (RFC 1951). The workspace adds no dependencies
//! for a pure crate, so this module holds a small decoder of its own. It is
//! written for clarity, not speed: a shader's code is a few kilobytes and is
//! unpacked once.
//!
//! The decoder never trusts the stream. It takes a limit on the output size
//! and stops with an error when the stream would exceed it, so that a damaged
//! size field cannot make it allocate without bound.

/// Why a stream could not be unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    /// The stream ends before its last block or before its checksum.
    Truncated,
    /// The two header bytes do not describe a DEFLATE stream with a window of
    /// at most 32 KiB, or their check bits are wrong.
    BadHeader,
    /// The stream asks for a preset dictionary, which shaders never use.
    PresetDictionary,
    /// A block has the reserved type, or a stored block whose two length
    /// fields disagree.
    BadBlock,
    /// A block's code lengths do not describe a usable prefix code, or the
    /// data holds a bit pattern that is no code.
    BadCode,
    /// A match reaches back before the start of the output.
    BadDistance,
    /// The output would be larger than the caller's limit.
    TooLarge,
    /// The Adler-32 checksum at the end does not match the output.
    BadChecksum,
}

impl std::fmt::Display for InflateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            InflateError::Truncated => "the stream is cut short",
            InflateError::BadHeader => "the zlib header is wrong",
            InflateError::PresetDictionary => "the stream needs a preset dictionary",
            InflateError::BadBlock => "a block header is wrong",
            InflateError::BadCode => "a prefix code is wrong",
            InflateError::BadDistance => "a match reaches before the start of the output",
            InflateError::TooLarge => "the output exceeds the size limit",
            InflateError::BadChecksum => "the checksum does not match",
        };
        f.write_str(text)
    }
}

impl std::error::Error for InflateError {}

/// The longest code DEFLATE allows, in bits.
const MAX_CODE_BITS: usize = 15;
/// The number of literal and length symbols, including two that never occur.
const LITERAL_SYMBOLS: usize = 288;
/// The number of distance symbols, including two that never occur.
const DISTANCE_SYMBOLS: usize = 32;
/// The symbol that ends a block.
const END_OF_BLOCK: u16 = 256;

/// The shortest match length of each length symbol, from symbol 257 on.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
/// The number of extra bits that follow each length symbol.
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// The shortest distance of each distance symbol.
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// The number of extra bits that follow each distance symbol.
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order in which a dynamic block stores the lengths of its length code.
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Computes the Adler-32 checksum of `data`, the checksum a zlib stream ends
/// with.
pub fn adler32(data: &[u8]) -> u32 {
    const MODULUS: u32 = 65521;
    let (mut low, mut high) = (1u32, 0u32);
    // 5552 is the most bytes that can be summed before the sums must be
    // reduced to stay within 32 bits.
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            low += u32::from(byte);
            high += low;
        }
        low %= MODULUS;
        high %= MODULUS;
    }
    (high << 16) | low
}

/// Unpacks a zlib stream: checks its header, inflates its DEFLATE data and
/// checks its checksum. Bytes after the checksum are ignored. `limit` is the
/// largest output the caller accepts.
pub fn zlib(stream: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let (&method, rest) = stream.split_first().ok_or(InflateError::Truncated)?;
    let (&flags, body) = rest.split_first().ok_or(InflateError::Truncated)?;
    let deflate = method & 0x0F == 8;
    let window_ok = method >> 4 <= 7;
    let check_ok = (u16::from(method) << 8 | u16::from(flags)).is_multiple_of(31);
    if !(deflate && window_ok && check_ok) {
        return Err(InflateError::BadHeader);
    }
    if flags & 0x20 != 0 {
        return Err(InflateError::PresetDictionary);
    }
    let (out, used) = deflate_raw(body, limit)?;
    let sum = body.get(used..used + 4).ok_or(InflateError::Truncated)?;
    let stored = u32::from_be_bytes([sum[0], sum[1], sum[2], sum[3]]);
    if stored != adler32(&out) {
        return Err(InflateError::BadChecksum);
    }
    Ok(out)
}

/// Inflates DEFLATE data that has no wrapper. Returns the output and the
/// number of input bytes the data occupied, counting the partly used last
/// byte. `limit` is the largest output the caller accepts.
pub fn deflate_raw(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), InflateError> {
    let mut bits = BitReader {
        data,
        at: 0,
        hold: 0,
        held: 0,
    };
    let mut out = Vec::new();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => stored_block(&mut bits, &mut out, limit)?,
            1 => {
                let (literals, distances) = fixed_codes();
                coded_block(&mut bits, &mut out, limit, &literals, &distances)?;
            }
            2 => {
                let (literals, distances) = dynamic_codes(&mut bits)?;
                coded_block(&mut bits, &mut out, limit, &literals, &distances)?;
            }
            _ => return Err(InflateError::BadBlock),
        }
        if last {
            return Ok((out, bits.at));
        }
    }
}

/// Reads bits from the input, least significant bit of each byte first, as
/// DEFLATE packs them.
struct BitReader<'a> {
    data: &'a [u8],
    /// The index of the next byte not yet taken into `hold`.
    at: usize,
    /// Bits taken from the input and not yet used, lowest first.
    hold: u32,
    /// How many bits of `hold` are valid.
    held: u32,
}

impl BitReader<'_> {
    /// Takes `count` bits (at most 16) as a number, first bit lowest.
    fn take(&mut self, count: u32) -> Result<u32, InflateError> {
        while self.held < count {
            let byte = *self.data.get(self.at).ok_or(InflateError::Truncated)?;
            self.at += 1;
            self.hold |= u32::from(byte) << self.held;
            self.held += 8;
        }
        let value = self.hold & ((1u32 << count) - 1);
        self.hold >>= count;
        self.held -= count;
        Ok(value)
    }

    /// Drops the unused bits of the current byte, so that the next read
    /// starts on a byte boundary.
    fn align(&mut self) {
        self.hold = 0;
        self.held = 0;
    }
}

/// A prefix code in canonical form: how many codes there are of each length,
/// and the symbols in order of their codes.
struct PrefixCode {
    counts: [u16; MAX_CODE_BITS + 1],
    symbols: Vec<u16>,
}

impl PrefixCode {
    /// Builds the code in which symbol `i` has a code of `lengths[i]` bits
    /// (0 meaning the symbol does not occur). Lengths that would need more
    /// codes than exist are rejected. A code that leaves some bit patterns
    /// unused is accepted, as DEFLATE allows for a lone distance code; such a
    /// pattern is rejected when it is met in the data.
    fn new(lengths: &[u8]) -> Result<Self, InflateError> {
        let mut counts = [0u16; MAX_CODE_BITS + 1];
        for &length in lengths {
            counts[usize::from(length)] += 1;
        }
        let mut free: i32 = 1;
        for &count in &counts[1..] {
            free = free * 2 - i32::from(count);
            if free < 0 {
                return Err(InflateError::BadCode);
            }
        }
        let mut next = [0u16; MAX_CODE_BITS + 2];
        for bits in 1..=MAX_CODE_BITS {
            next[bits + 1] = next[bits] + counts[bits];
        }
        let used = usize::from(next[MAX_CODE_BITS + 1]);
        let mut symbols = vec![0u16; used];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                let slot = &mut next[usize::from(length)];
                symbols[usize::from(*slot)] = symbol as u16;
                *slot += 1;
            }
        }
        Ok(PrefixCode { counts, symbols })
    }

    /// Reads one symbol, a bit at a time: at each length the codes of that
    /// length are a run of consecutive numbers, so a code is recognised as
    /// soon as it falls inside the run for its length.
    fn read(&self, bits: &mut BitReader<'_>) -> Result<u16, InflateError> {
        let (mut code, mut first, mut index) = (0u32, 0u32, 0u32);
        for length in 1..=MAX_CODE_BITS {
            code |= bits.take(1)?;
            let count = u32::from(self.counts[length]);
            if code < first + count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(InflateError::BadCode)
    }
}

/// Appends bytes to the output unless that would exceed the limit.
fn room(out: &[u8], more: usize, limit: usize) -> Result<(), InflateError> {
    if limit.saturating_sub(out.len()) < more {
        return Err(InflateError::TooLarge);
    }
    Ok(())
}

/// Copies a block that is stored without compression.
fn stored_block(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), InflateError> {
    bits.align();
    let header = bits
        .data
        .get(bits.at..bits.at + 4)
        .ok_or(InflateError::Truncated)?;
    let length = u16::from_le_bytes([header[0], header[1]]);
    let inverse = u16::from_le_bytes([header[2], header[3]]);
    if length != !inverse {
        return Err(InflateError::BadBlock);
    }
    let start = bits.at + 4;
    let end = start + usize::from(length);
    let bytes = bits.data.get(start..end).ok_or(InflateError::Truncated)?;
    room(out, bytes.len(), limit)?;
    out.extend_from_slice(bytes);
    bits.at = end;
    Ok(())
}

/// Builds the two codes every fixed block uses.
fn fixed_codes() -> (PrefixCode, PrefixCode) {
    let mut lengths = [8u8; LITERAL_SYMBOLS];
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    let literals = PrefixCode::new(&lengths);
    let distances = PrefixCode::new(&[5u8; DISTANCE_SYMBOLS]);
    match (literals, distances) {
        (Ok(literals), Ok(distances)) => (literals, distances),
        // The fixed lengths are constants that form complete codes, so
        // building them cannot fail. Empty codes keep this function total;
        // any data read with them is rejected as a bad code.
        _ => (empty_code(), empty_code()),
    }
}

/// A code with no symbols, which rejects every bit pattern.
fn empty_code() -> PrefixCode {
    PrefixCode {
        counts: [0; MAX_CODE_BITS + 1],
        symbols: Vec::new(),
    }
}

/// Reads the two codes a dynamic block starts with.
fn dynamic_codes(bits: &mut BitReader<'_>) -> Result<(PrefixCode, PrefixCode), InflateError> {
    let literal_count = bits.take(5)? as usize + 257;
    let distance_count = bits.take(5)? as usize + 1;
    let length_count = bits.take(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err(InflateError::BadCode);
    }
    let mut code_lengths = [0u8; 19];
    for &slot in &CODE_LENGTH_ORDER[..length_count] {
        code_lengths[slot] = bits.take(3)? as u8;
    }
    let length_code = PrefixCode::new(&code_lengths)?;

    let total = literal_count + distance_count;
    let mut lengths = [0u8; LITERAL_SYMBOLS + DISTANCE_SYMBOLS];
    let mut filled = 0;
    while filled < total {
        let symbol = length_code.read(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => {
                let previous = *lengths
                    .get(filled.wrapping_sub(1))
                    .ok_or(InflateError::BadCode)?;
                (previous, 3 + bits.take(2)? as usize)
            }
            17 => (0, 3 + bits.take(3)? as usize),
            _ => (0, 11 + bits.take(7)? as usize),
        };
        if filled + repeat > total {
            return Err(InflateError::BadCode);
        }
        lengths[filled..filled + repeat].fill(value);
        filled += repeat;
    }
    if lengths[usize::from(END_OF_BLOCK)] == 0 {
        return Err(InflateError::BadCode);
    }
    let literals = PrefixCode::new(&lengths[..literal_count])?;
    let distances = PrefixCode::new(&lengths[literal_count..total])?;
    Ok((literals, distances))
}

/// Decodes one compressed block: literals, and matches that repeat earlier
/// output.
fn coded_block(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    limit: usize,
    literals: &PrefixCode,
    distances: &PrefixCode,
) -> Result<(), InflateError> {
    loop {
        let symbol = literals.read(bits)?;
        if symbol < END_OF_BLOCK {
            room(out, 1, limit)?;
            out.push(symbol as u8);
            continue;
        }
        if symbol == END_OF_BLOCK {
            return Ok(());
        }
        let slot = usize::from(symbol - 257);
        let base = *LENGTH_BASE.get(slot).ok_or(InflateError::BadCode)?;
        let length = usize::from(base) + bits.take(u32::from(LENGTH_EXTRA[slot]))? as usize;

        let slot = usize::from(distances.read(bits)?);
        let base = *DISTANCE_BASE.get(slot).ok_or(InflateError::BadCode)?;
        let distance = usize::from(base) + bits.take(u32::from(DISTANCE_EXTRA[slot]))? as usize;
        if distance > out.len() {
            return Err(InflateError::BadDistance);
        }
        room(out, length, limit)?;
        // A match may overlap its own output (a distance shorter than the
        // length repeats a pattern), so the copy goes a byte at a time.
        let from = out.len() - distance;
        for step in 0..length {
            let byte = out[from + step];
            out.push(byte);
        }
    }
}

/// Wraps `data` in a zlib stream of stored blocks. It compresses nothing; it
/// exists so that tests can build a stream this module must unpack.
#[cfg(test)]
pub(crate) fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(0xFFFF).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hello hello hello hello` packed by zlib at its highest level: one
    /// block with the fixed codes, holding a match that overlaps itself.
    const FIXED: [u8; 16] = [
        0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03, 0x08,
        0xb1,
    ];

    /// The text of [`dynamic_text`] packed by zlib at its highest level: one
    /// block with codes of its own.
    const DYNAMIC: [u8; 188] = [
        0x78, 0xda, 0x7d, 0xd3, 0x5b, 0x0a, 0x02, 0x31, 0x0c, 0x85, 0xe1, 0x77, 0x57, 0xd1, 0x25,
        0x34, 0x39, 0xbd, 0xba, 0x1c, 0x65, 0xc4, 0xc1, 0x61, 0x44, 0x51, 0x74, 0xf9, 0x22, 0xe9,
        0x63, 0x7a, 0x9e, 0x7f, 0x02, 0x4d, 0x3e, 0xba, 0xad, 0xfb, 0x12, 0xe2, 0x31, 0xbc, 0xae,
        0x4b, 0x78, 0xbc, 0xd7, 0xf3, 0x2d, 0x9c, 0x9e, 0xf7, 0xcf, 0x1e, 0x2e, 0xf7, 0x6f, 0x88,
        0x87, 0xed, 0x5f, 0xc5, 0xaf, 0xd5, 0xaa, 0xfa, 0x55, 0xac, 0xc2, 0xaf, 0xcd, 0x6a, 0xf2,
        0xab, 0x5a, 0xcd, 0x7e, 0xed, 0x56, 0x8b, 0x5f, 0x61, 0xb5, 0x4e, 0x5e, 0x35, 0x56, 0x6a,
        0x7e, 0x4e, 0x56, 0xfb, 0x64, 0x78, 0xec, 0x24, 0x93, 0x73, 0xe5, 0x91, 0x27, 0xf7, 0x92,
        0xb1, 0x96, 0x4c, 0x2e, 0x56, 0x46, 0x06, 0xc7, 0x48, 0x54, 0x43, 0x32, 0xe5, 0x90, 0x42,
        0x3d, 0xa4, 0x52, 0x10, 0x69, 0x54, 0x44, 0x3a, 0x25, 0xd1, 0xc8, 0x4d, 0x54, 0x28, 0x8a,
        0x2a, 0x57, 0x51, 0x50, 0x15, 0x4d, 0x5c, 0x45, 0x33, 0x55, 0xd1, 0x42, 0x55, 0xb4, 0xf2,
        0x3f, 0xd2, 0xa8, 0x8a, 0x76, 0xaa, 0x82, 0x48, 0x55, 0x20, 0x54, 0x05, 0x4a, 0x55, 0x00,
        0xae, 0x82, 0x44, 0x55, 0x90, 0xb9, 0x0a, 0x0a, 0x55, 0x41, 0xe5, 0x2a, 0x68, 0x54, 0x05,
        0x7d, 0xa6, 0xf2, 0x03, 0x0f, 0xbb, 0x93, 0xc1,
    ];

    /// `abc` in one stored block.
    const STORED: [u8; 14] = [
        0x78, 0x01, 0x01, 0x03, 0x00, 0xfc, 0xff, 0x61, 0x62, 0x63, 0x02, 0x4d, 0x01, 0x27,
    ];

    /// The text [`DYNAMIC`] was made from.
    fn dynamic_text() -> Vec<u8> {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("line {i}: the quick brown fox {}\n", i * 7 % 13));
        }
        text.into_bytes()
    }

    #[test]
    fn checksum_matches_the_published_example() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn unpacks_each_block_type() {
        assert_eq!(zlib(&STORED, 16).unwrap(), b"abc");
        assert_eq!(zlib(&FIXED, 64).unwrap(), b"hello hello hello hello");
        assert_eq!(zlib(&DYNAMIC, 4096).unwrap(), dynamic_text());
    }

    #[test]
    fn unpacks_what_the_test_packer_makes() {
        for size in [0usize, 1, 5, 0xFFFF, 0x1_0000, 0x2_0005] {
            let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
            assert_eq!(zlib(&zlib_stored(&data), size).unwrap(), data);
        }
    }

    #[test]
    fn enforces_the_output_limit() {
        assert_eq!(zlib(&STORED, 2), Err(InflateError::TooLarge));
        assert_eq!(zlib(&FIXED, 22), Err(InflateError::TooLarge));
        assert_eq!(zlib(&DYNAMIC, 1238), Err(InflateError::TooLarge));
        assert!(zlib(&DYNAMIC, 1239).is_ok());
    }

    #[test]
    fn rejects_damaged_streams() {
        assert_eq!(zlib(&[], 16), Err(InflateError::Truncated));
        assert_eq!(zlib(&[0x78], 16), Err(InflateError::Truncated));
        assert_eq!(zlib(&[0x79, 0xda, 0], 16), Err(InflateError::BadHeader));
        assert_eq!(zlib(&[0x78, 0xdb, 0], 16), Err(InflateError::BadHeader));
        // 0x78 0x20 has a correct check but asks for a dictionary.
        assert_eq!(
            zlib(&[0x78, 0x20, 0], 16),
            Err(InflateError::PresetDictionary)
        );
        // Block type 3 is reserved.
        assert_eq!(zlib(&[0x78, 0x01, 0x07], 16), Err(InflateError::BadBlock));

        let mut stream = STORED;
        stream[5] ^= 1;
        assert_eq!(zlib(&stream, 16), Err(InflateError::BadBlock));

        let mut stream = FIXED;
        stream[15] ^= 1;
        assert_eq!(zlib(&stream, 64), Err(InflateError::BadChecksum));

        // A fixed block that starts with a match has nothing to copy from:
        // length symbol 257 (the seven bits 0000001), then distance symbol 0.
        assert_eq!(
            zlib(&[0x78, 0x01, 0x03, 0x02, 0x00, 0x00], 16),
            Err(InflateError::BadDistance)
        );
    }

    #[test]
    fn truncation_at_every_length_is_an_error() {
        for stream in [&STORED[..], &FIXED[..], &DYNAMIC[..]] {
            for length in 0..stream.len() {
                assert!(zlib(&stream[..length], 4096).is_err(), "length {length}");
            }
        }
    }

    #[test]
    fn every_single_byte_change_is_survived() {
        // Not every change is detectable before the checksum, but none may
        // panic or run past the limit.
        for at in 0..DYNAMIC.len() {
            for flip in [0x01u8, 0x10, 0x80, 0xFF] {
                let mut stream = DYNAMIC;
                stream[at] ^= flip;
                if let Ok(out) = zlib(&stream, 4096) {
                    assert!(out.len() <= 4096);
                }
            }
        }
    }
}
