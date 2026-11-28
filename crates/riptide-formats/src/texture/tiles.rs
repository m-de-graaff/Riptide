//! How a texture in a 64 KiB mode divides into 64 KiB tiles.
//!
//! A title that maps a texture's memory piece by piece (a "tiled" or
//! "reserved" resource) asks the console's library which tiles each mip
//! level takes. A tile is one 64 KiB block of the layout.

use super::{Error, PlaneLayout};

/// The size of one tile in bytes.
const TILE_BYTES: u64 = 64 << 10;

/// The tiles of one standard mip level of one slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TileRegion {
    /// Width of the level in tiles.
    pub width: u32,
    /// Height of the level in tiles.
    pub height: u32,
    /// Depth of the level in tiles: 1 for a listed level.
    pub depth: u32,
    /// The index of the level's first tile, counted from the start of the
    /// plane.
    pub start: u32,
}

/// The division of a plane into 64 KiB tiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileMap {
    /// Tiles in the whole plane.
    pub total_tiles: u32,
    /// Mip levels that have tiles of their own.
    pub standard_mips: u32,
    /// Mip levels packed together in the mip tail.
    pub packed_mips: u32,
    /// Tiles the packed levels take in one slice: one when the texture has
    /// both standard levels and a tail, the whole slice when every level is
    /// in the tail, none without a tail.
    pub packed_tiles: u32,
    /// The index of the first packed tile in a slice. The mip tail starts
    /// the slice, so this is 0.
    pub packed_start: u32,
    /// Width and height of a tile in texels, and its depth (1).
    pub tile_texels: [u32; 3],
    /// One entry per standard level per slice, slices in order and within a
    /// slice the largest level first.
    ///
    /// For a texture with a mip tail the console's library lists at most two
    /// slices and fills every entry after the first slice with zeroes. That
    /// was observed, not explained; the list here does the same so that a
    /// title sees what it would on the console.
    pub regions: Vec<TileRegion>,
}

impl PlaneLayout {
    /// The division of the plane into 64 KiB tiles.
    ///
    /// Fails with [`Error::NotTiled`] when the mode is not a 64 KiB mode (the
    /// console's library fails there too), and with [`Error::Unsupported`]
    /// for a thick volume, whose tile map is not built.
    pub fn tile_map(&self) -> Result<TileMap, Error> {
        if self.alignment() != TILE_BYTES {
            return Err(Error::NotTiled);
        }
        if self.is_thick() {
            return Err(Error::Unsupported);
        }
        let mips = self.desc().mips;
        let standard = self.first_tail_mip();
        let has_tail = standard < mips;
        let packed_bytes = match standard {
            _ if !has_tail => 0,
            0 => self.slice_bytes(),
            // The smallest standard level follows the tail directly.
            _ => self.mips()[standard as usize - 1].offset,
        };
        let [bw, bh, bd] = self.block();
        let texels = self.desc().element_texels;
        let listed_slices = if has_tail {
            self.stored_slices().min(2)
        } else {
            self.stored_slices()
        };
        let mut regions = Vec::with_capacity((standard * listed_slices) as usize);
        for slice in 0..listed_slices {
            for level in &self.mips()[..standard as usize] {
                regions.push(if has_tail && slice > 0 {
                    TileRegion::default()
                } else {
                    let start = u64::from(slice) * self.slice_bytes() + level.offset;
                    TileRegion {
                        width: level.pitch / bw,
                        height: level.rows / bh,
                        depth: 1,
                        start: (start / TILE_BYTES) as u32,
                    }
                });
            }
        }
        Ok(TileMap {
            total_tiles: (self.bytes() / TILE_BYTES) as u32,
            standard_mips: standard,
            packed_mips: mips - standard,
            packed_tiles: (packed_bytes / TILE_BYTES) as u32,
            packed_start: 0,
            tile_texels: [bw * texels, bh * texels, bd],
            regions,
        })
    }
}
