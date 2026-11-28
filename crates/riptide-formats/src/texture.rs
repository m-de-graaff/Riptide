//! How a texture lies in the memory of the console's GPU.
//!
//! The console GPU (AMD RDNA 2) does not store a texture row after row. It
//! cuts it into blocks of 256 bytes, 4 KiB or 64 KiB and, inside a block,
//! interleaves the bits of the x, y (and for volumes z) coordinates into the
//! byte address, so that texels close together on screen are close together
//! in memory. Which block size and which interleaving a texture uses is its
//! *mode*. A title asks the console's layout library for the size of a
//! texture in a mode, where every mip level starts, and to copy rows into and
//! out of that layout. This module is the arithmetic behind those answers,
//! as pure functions with no state, no dependencies and no unsafe code.
//!
//! # Words used here
//!
//! * An **element** is what the layout addresses: one texel, or one 4x4 block
//!   of a block-compressed format.
//! * A **plane** is one array of elements. Colour textures have one. Depth
//!   formats with stencil have two (depth, then stencil), because the GPU
//!   keeps them apart.
//! * A **slice** is one array layer (a cube is six of them) or one depth
//!   layer of a volume.
//! * The **mip tail** is the group of small mip levels that share a single
//!   block instead of taking a block each.
//! * A mode is **thick** when its blocks span several depth layers of a
//!   volume. The stored unit is then a *slab* of that many layers.
//!
//! Inside one slice (or slab) the whole mip chain is stored together, the
//! smallest levels first: the mip tail at offset 0, the largest level last.
//! Slices follow one another.
//!
//! # Where the numbers come from
//!
//! The layouts are the ones AMD's address library describes for this GPU
//! generation ("GFX10", published in Mesa under the MIT licence): block
//! dimensions, padding, the order of levels, the mip tail rules and the bit
//! interleaving of the modes without XOR. The tests compare sizes and the
//! address of every element against values that library produced.
//!
//! # What is built and what is not
//!
//! Sizes, alignments, mip offsets and mip tails are computed for every mode
//! the GPU has: row-major, and the 256-byte, 4 KiB and 64 KiB families in
//! their standard, display, render and depth variants, with and without XOR,
//! for 1-, 2-, 4-, 8- and 16-byte elements, block compression, arrays, cubes,
//! volumes and multisampling.
//!
//! Element addresses and copies are built for row-major and for the six
//! modes without XOR (256 B, 4 KiB and 64 KiB, standard and display),
//! single-sampled. They are **not** built for the XOR modes (the `X` and `T`
//! variants, which include every render and depth mode) nor for multisampled
//! textures: those addresses also depend on the GPU's pipe and bank
//! configuration, which differs between the two consoles and which nothing
//! here knows. Asking for them returns [`Error::Unsupported`]. Titles use
//! those modes for render targets and depth buffers, which a host renders
//! itself, so nothing has needed their bytes yet.
//!
//! No compression metadata planes (the GPU's hierarchical depth, colour and
//! coverage masks) are laid out. The console adds them to render targets and
//! depth buffers, so those come out smaller here than on the console.
//!
//! One-dimensional textures in a tiled mode are laid out as two-dimensional
//! textures one element high. That is an assumption: the address library
//! only gives one-dimensional textures the row-major mode, and that is the
//! only case checked against it.

mod copy;
mod format;
mod layout;
mod mode;
mod tiles;

#[cfg(test)]
mod tests;

pub use copy::LinearPitch;
pub use format::{
    FormatInfo, Plane, PlaneRole, Texture, TextureDesc, Usage, format_info, suggested_mode,
};
pub use layout::{MAX_EXTENT, MAX_MIPS, MipLevel, PlaneDesc, PlaneLayout, TailPlace};
pub use mode::{Family, Mode};
pub use tiles::{TileMap, TileRegion};

/// The shape of a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shape {
    /// A row of texels, possibly an array of rows.
    OneD,
    /// A rectangle of texels, possibly an array of them. A cube is an array
    /// of six, a cube array a multiple of six.
    TwoD,
    /// A volume. Its depth halves with each mip level, like its width and
    /// height.
    ThreeD,
}

/// Why a question could not be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Error {
    /// The description is not a texture the GPU can have: a zero or too
    /// large size, an element size that does not exist, an unknown format,
    /// a mode the shape cannot use, or multisampling where it is not allowed.
    Invalid,
    /// A plane, mip level, slice, coordinate or sample is outside the
    /// texture.
    OutOfRange,
    /// The layout is valid and its sizes are known, but the answer needs
    /// addressing that is not built: an XOR mode, a multisampled texture, or
    /// the 64 KiB tile map of a thick volume.
    Unsupported,
    /// A buffer is too small for the layout or for the rows, or a pitch is
    /// smaller than the data it must hold.
    TooSmall,
    /// The 64 KiB tile map was asked of a texture whose mode is not one of
    /// the 64 KiB modes.
    NotTiled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Invalid => "the description is not a texture the console GPU can have",
            Error::OutOfRange => "the plane, mip level, slice or coordinate is outside the texture",
            Error::Unsupported => "addressing for this mode is not built",
            Error::TooSmall => "a buffer or pitch is too small",
            Error::NotTiled => "the texture is not in a 64 KiB mode",
        })
    }
}

impl std::error::Error for Error {}
