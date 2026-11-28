//! The layout modes of the console GPU and what each one's number means.

/// What a mode is meant for. The family decides the bit interleaving inside
/// a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// Rows one after another, no blocks.
    Linear,
    /// The general-purpose interleaving, used for sampled textures.
    Standard,
    /// The interleaving the display hardware can scan out.
    Display,
    /// The interleaving for render targets.
    Render,
    /// The interleaving for depth and stencil buffers.
    Depth,
}

/// A layout mode ("swizzle mode") of the console GPU.
///
/// The names give the family, the block size, and a suffix: `X` for the
/// variants that mix the GPU's pipe and bank bits into the address, `T` for
/// the 64 KiB variants that do the same for partially resident textures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Row-major: number 0.
    Linear,
    /// 256-byte blocks, standard: number 1.
    Standard256,
    /// 256-byte blocks, display: number 2.
    Display256,
    /// 4 KiB blocks, standard: number 5.
    Standard4K,
    /// 4 KiB blocks, display: number 6.
    Display4K,
    /// 64 KiB blocks, standard: number 9.
    Standard64K,
    /// 64 KiB blocks, display: number 10.
    Display64K,
    /// 64 KiB blocks, standard, XOR for partial residency: number 17.
    Standard64KT,
    /// 64 KiB blocks, display, XOR for partial residency: number 18.
    Display64KT,
    /// 4 KiB blocks, standard, XOR: number 21.
    Standard4KX,
    /// 4 KiB blocks, display, XOR: number 22.
    Display4KX,
    /// 4 KiB blocks, render, XOR: number 23.
    Render4KX,
    /// 64 KiB blocks, depth, XOR: number 24.
    Depth64KX,
    /// 64 KiB blocks, standard, XOR: number 25.
    Standard64KX,
    /// 64 KiB blocks, display, XOR: number 26.
    Display64KX,
    /// 64 KiB blocks, render, XOR: number 27.
    Render64KX,
}

impl Mode {
    /// Every mode, in the order of their numbers.
    pub const ALL: [Mode; 16] = [
        Mode::Linear,
        Mode::Standard256,
        Mode::Display256,
        Mode::Standard4K,
        Mode::Display4K,
        Mode::Standard64K,
        Mode::Display64K,
        Mode::Standard64KT,
        Mode::Display64KT,
        Mode::Standard4KX,
        Mode::Display4KX,
        Mode::Render4KX,
        Mode::Depth64KX,
        Mode::Standard64KX,
        Mode::Display64KX,
        Mode::Render64KX,
    ];

    /// The mode with the hardware number `number`, or `None` for numbers
    /// this GPU generation leaves unused.
    ///
    /// The console's resource description carries this number in the low
    /// byte of its layout field.
    pub fn from_number(number: u32) -> Option<Mode> {
        Mode::ALL.into_iter().find(|mode| mode.number() == number)
    }

    /// The hardware number of the mode.
    pub fn number(self) -> u32 {
        match self {
            Mode::Linear => 0,
            Mode::Standard256 => 1,
            Mode::Display256 => 2,
            Mode::Standard4K => 5,
            Mode::Display4K => 6,
            Mode::Standard64K => 9,
            Mode::Display64K => 10,
            Mode::Standard64KT => 17,
            Mode::Display64KT => 18,
            Mode::Standard4KX => 21,
            Mode::Display4KX => 22,
            Mode::Render4KX => 23,
            Mode::Depth64KX => 24,
            Mode::Standard64KX => 25,
            Mode::Display64KX => 26,
            Mode::Render64KX => 27,
        }
    }

    /// What the mode is meant for.
    pub fn family(self) -> Family {
        match self {
            Mode::Linear => Family::Linear,
            Mode::Standard256
            | Mode::Standard4K
            | Mode::Standard64K
            | Mode::Standard64KT
            | Mode::Standard4KX
            | Mode::Standard64KX => Family::Standard,
            Mode::Display256
            | Mode::Display4K
            | Mode::Display64K
            | Mode::Display64KT
            | Mode::Display4KX
            | Mode::Display64KX => Family::Display,
            Mode::Render4KX | Mode::Render64KX => Family::Render,
            Mode::Depth64KX => Family::Depth,
        }
    }

    /// The base-2 logarithm of the block size in bytes: 8, 12 or 16, and 0
    /// for the row-major mode, which has no blocks.
    pub fn block_log2(self) -> u32 {
        match self {
            Mode::Linear => 0,
            Mode::Standard256 | Mode::Display256 => 8,
            Mode::Standard4K
            | Mode::Display4K
            | Mode::Standard4KX
            | Mode::Display4KX
            | Mode::Render4KX => 12,
            Mode::Standard64K
            | Mode::Display64K
            | Mode::Standard64KT
            | Mode::Display64KT
            | Mode::Depth64KX
            | Mode::Standard64KX
            | Mode::Display64KX
            | Mode::Render64KX => 16,
        }
    }

    /// The block size in bytes: 256, 4096 or 65536, and 1 for the row-major
    /// mode.
    pub fn block_bytes(self) -> u64 {
        1 << self.block_log2()
    }

    /// Whether the mode mixes the GPU's pipe and bank bits into addresses
    /// (the `X` and `T` variants). Sizes of such a texture are known here;
    /// the addresses of its elements are not.
    pub fn has_xor(self) -> bool {
        self.number() >= 16
    }

    /// The mode number the console's library reports for a mip level that
    /// lies in the mip tail: the 256-byte mode of the same family (1 for
    /// standard, 2 for display, 3 for render, a number the GPU itself leaves
    /// unused), and the depth mode's own number for depth.
    pub fn tail_number(self) -> u32 {
        match self.family() {
            Family::Display => Mode::Display256.number(),
            Family::Render => 3,
            Family::Depth => self.number(),
            Family::Linear | Family::Standard => Mode::Standard256.number(),
        }
    }
}
