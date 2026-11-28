//! The layout of one plane: block dimensions, where each mip level lies, and
//! the address of one element.

use super::{Error, Family, Mode, Shape};

/// The largest width, height, and depth or slice count accepted, in texels.
///
/// The GPU's image descriptors hold at most this many texels per side, so no
/// texture it can use is refused. The bound also keeps every size computed
/// here far inside 64 bits, whatever numbers a title passes.
pub const MAX_EXTENT: u32 = 1 << 14;

/// The largest number of mip levels accepted. A texture of [`MAX_EXTENT`]
/// texels has 15 levels; one more is tolerated and laid out like the others.
pub const MAX_MIPS: u32 = 16;

/// One plane of a texture, in the terms the layout needs.
///
/// [`super::Texture`] builds these from a format number; build one directly
/// to lay out a plane of some other element size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlaneDesc {
    /// The shape of the texture.
    pub shape: Shape,
    /// Width of the largest mip level in texels.
    pub width: u32,
    /// Height of the largest mip level in texels (1 for [`Shape::OneD`]).
    pub height: u32,
    /// Depth of a volume in texels; for the other shapes the number of array
    /// slices (six for a cube).
    pub depth_or_slices: u32,
    /// Number of mip levels, at least 1.
    pub mips: u32,
    /// Bytes in one element: 1, 2, 4, 8 or 16.
    pub element_bytes: u32,
    /// Texels along one side of an element: 1, or 4 for block compression.
    pub element_texels: u32,
    /// Samples per texel: 1, 2, 4 or 8. More than one is only allowed for a
    /// two-dimensional texture with a single mip level in a tiled mode.
    pub samples: u32,
    /// The layout mode.
    pub mode: Mode,
}

/// Where a mip level sits inside the block that holds the mip tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TailPlace {
    /// The level's nominal byte offset inside the tail block. In a thin mode
    /// this is the address of the level's first element. In a thick mode the
    /// level is found by position instead, and this number only names its
    /// place in the tail's order.
    pub offset: u32,
    /// The element column inside the tail block at which the level starts.
    pub x: u32,
    /// The element row inside the tail block at which the level starts.
    pub y: u32,
}

/// One mip level of a plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MipLevel {
    /// Width of the level's data in elements.
    pub width: u32,
    /// Height of the level's data in elements.
    pub height: u32,
    /// Depth of the level's data in texels; 1 unless the texture is a volume.
    pub depth: u32,
    /// Width of the level in texels.
    pub texel_width: u32,
    /// Height of the level in texels.
    pub texel_height: u32,
    /// Elements from one stored row to the next: the width padded to whole
    /// blocks, or for a level in the mip tail its share of the tail block.
    pub pitch: u32,
    /// Stored rows: the height padded the same way.
    pub rows: u32,
    /// Stored depth layers of a volume: the depth padded to whole blocks in
    /// a thick mode, the full depth of the texture in the other modes (every
    /// depth layer then holds the whole chain). 1 unless the texture is a
    /// volume.
    pub depth_pitch: u32,
    /// Byte offset of the level from the start of its slice or slab. For a
    /// level in the mip tail of a thin mode this is its offset inside the
    /// tail block, which lies at the start of the slice. For a level in the
    /// mip tail of a thick mode this is 0, the start of the tail block: such
    /// a level is placed by [`TailPlace::x`] and [`TailPlace::y`].
    pub offset: u64,
    /// Bytes of one stored layer of the level, as the console's library
    /// reports them: pitch times rows times element size times samples, or
    /// for a tail level of a thick mode one layer's share of the tail block.
    pub slice_bytes: u64,
    /// [`MipLevel::slice_bytes`] times [`MipLevel::depth_pitch`].
    pub bytes: u64,
    /// Where the level sits in the mip tail, if it is in it.
    pub tail: Option<TailPlace>,
}

impl MipLevel {
    /// Whether the level shares the mip tail's block.
    pub fn in_tail(&self) -> bool {
        self.tail.is_some()
    }
}

/// The address bit each coordinate bit takes inside the first 256 bytes of a
/// block, lowest coordinate bit first.
struct Micro {
    x: &'static [u8],
    y: &'static [u8],
    z: &'static [u8],
}

const fn micro(x: &'static [u8], y: &'static [u8], z: &'static [u8]) -> Micro {
    Micro { x, y, z }
}

/// Standard family, thin blocks, by the base-2 logarithm of the element size.
const STANDARD_THIN: [Micro; 5] = [
    micro(&[0, 1, 2, 3], &[4, 5, 6, 7], &[]),
    micro(&[1, 2, 3, 7], &[4, 5, 6], &[]),
    micro(&[2, 3, 7], &[4, 5, 6], &[]),
    micro(&[3, 6, 7], &[4, 5], &[]),
    micro(&[6, 7], &[4, 5], &[]),
];

/// Display family, thin blocks.
const DISPLAY_THIN: [Micro; 5] = [
    micro(&[0, 1, 2, 6], &[4, 3, 5, 7], &[]),
    micro(&[1, 2, 3, 7], &[4, 5, 6], &[]),
    micro(&[2, 3, 6], &[4, 5, 7], &[]),
    micro(&[3, 5, 6], &[4, 7], &[]),
    micro(&[4, 6], &[5, 7], &[]),
];

/// Thick blocks of a volume, which the standard and display families share.
const THICK: [Micro; 5] = [
    micro(&[0, 1, 6], &[3, 5], &[2, 4, 7]),
    micro(&[1, 6], &[3, 5], &[2, 4, 7]),
    micro(&[2, 6], &[3, 5], &[4, 7]),
    micro(&[3, 6], &[5], &[4, 7]),
    micro(&[6], &[5], &[4, 7]),
];

/// For each axis, the address bits its coordinate bits take in a whole block
/// of `2^block_log2` bytes. Above the first 256 bytes the address bits go to
/// y and x in turn, and in a thick block to y, x and z in turn.
fn axis_bits(micro: &Micro, block_log2: u32, thick: bool) -> [Vec<u8>; 3] {
    let mut axes = [micro.x.to_vec(), micro.y.to_vec(), micro.z.to_vec()];
    let turn: &[usize] = if thick { &[1, 0, 2] } else { &[1, 0] };
    for (bit, &axis) in (8..block_log2 as u8).zip(turn.iter().cycle()) {
        axes[axis].push(bit);
    }
    axes
}

/// The byte offset inside a block that each coordinate of one axis
/// contributes, for every coordinate the block spans along that axis.
fn deposit(bits: &[u8]) -> Vec<u32> {
    (0..1u32 << bits.len())
        .map(|coordinate| {
            bits.iter()
                .enumerate()
                .map(|(index, &to)| ((coordinate >> index) & 1) << to)
                .sum()
        })
        .collect()
}

/// Width and height in elements of a thin block of `2^block_log2` bytes.
/// Width takes the extra factor of two when the count is odd, except that
/// an odd number of sample bits gives it to the height.
fn thin_block(block_log2: u32, element_log2: u32, samples_log2: u32) -> [u32; 2] {
    let elements_log2 = block_log2 - element_log2 - samples_log2;
    let width_first = samples_log2.is_multiple_of(2) || !block_log2.is_multiple_of(2);
    let width_log2 = (elements_log2 + u32::from(width_first)) / 2;
    [1 << width_log2, 1 << (elements_log2 - width_log2)]
}

/// How many mip levels the tail block of a mode can hold.
fn tail_capacity(block_log2: u32, thick: bool) -> u32 {
    let effective = if thick {
        block_log2 - (block_log2 - 8) / 3
    } else {
        block_log2
    };
    match effective {
        0..=8 => 0,
        9..=11 => 1 + (1 << (effective - 9)),
        _ => effective - 4,
    }
}

/// Elements along one side of a mip level's data: the texels halve rounding
/// down, to at least one, and then round up to whole elements.
fn data_extent(texels: u32, mip: u32, element_texels: u32) -> u32 {
    (texels >> mip).max(1).div_ceil(element_texels)
}

/// Elements along one side of a mip level as the layout places it: the
/// elements of the largest level halve rounding up. For odd sizes this can
/// be one more than [`data_extent`].
fn placed_extent(texels: u32, mip: u32, element_texels: u32) -> u32 {
    texels.div_ceil(element_texels).div_ceil(1 << mip)
}

/// The per-axis offset tables of an addressable tiled layout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spread {
    x: Vec<u32>,
    y: Vec<u32>,
    z: Vec<u32>,
}

/// The complete layout of one plane.
///
/// Offsets are from the start of the plane. Build one with
/// [`PlaneLayout::new`]; the values cannot be changed afterwards, so the
/// address and copy functions can rely on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneLayout {
    desc: PlaneDesc,
    block: [u32; 3],
    thick: bool,
    stores: u32,
    chain_bytes: u64,
    bytes: u64,
    alignment: u64,
    first_tail_mip: u32,
    mips: Vec<MipLevel>,
    spread: Option<Spread>,
}

impl PlaneLayout {
    /// Lays out the plane `desc` describes.
    ///
    /// Fails with [`Error::Invalid`] when the description is not a texture
    /// the GPU can have: see the fields of [`PlaneDesc`]. A volume can only
    /// use the row-major mode, the standard 4 KiB and 64 KiB modes, and the
    /// 64 KiB XOR modes.
    pub fn new(desc: &PlaneDesc) -> Result<PlaneLayout, Error> {
        let d = *desc;
        let extent = 1..=MAX_EXTENT;
        let sized = extent.contains(&d.width)
            && extent.contains(&d.height)
            && extent.contains(&d.depth_or_slices)
            && (1..=MAX_MIPS).contains(&d.mips);
        let element =
            matches!(d.element_bytes, 1 | 2 | 4 | 8 | 16) && matches!(d.element_texels, 1 | 4);
        let sampled = match d.samples {
            1 => true,
            2 | 4 | 8 => d.shape == Shape::TwoD && d.mips == 1 && d.mode != Mode::Linear,
            _ => false,
        };
        let shaped = match d.shape {
            Shape::OneD => d.height == 1,
            Shape::TwoD => true,
            Shape::ThreeD => matches!(
                d.mode,
                Mode::Linear
                    | Mode::Standard4K
                    | Mode::Standard64K
                    | Mode::Standard64KT
                    | Mode::Standard4KX
                    | Mode::Depth64KX
                    | Mode::Standard64KX
                    | Mode::Display64KX
                    | Mode::Render64KX
            ),
        };
        if !(sized && element && sampled && shaped) {
            return Err(Error::Invalid);
        }
        Ok(if d.mode == Mode::Linear {
            Self::row_major(d)
        } else {
            Self::tiled(d)
        })
    }

    /// The row-major layout: every row is padded to a multiple of 256 bytes,
    /// the levels follow one another smallest first, and every depth layer
    /// of a volume holds a whole chain.
    fn row_major(d: PlaneDesc) -> PlaneLayout {
        let bpe = u64::from(d.element_bytes);
        let pitch_step = 256 / d.element_bytes;
        let volume = d.shape == Shape::ThreeD;
        let mut mips: Vec<MipLevel> = (0..d.mips)
            .map(|mip| {
                let pitch =
                    placed_extent(d.width, mip, d.element_texels).next_multiple_of(pitch_step);
                let rows = placed_extent(d.height, mip, d.element_texels);
                let depth_pitch = if volume { d.depth_or_slices } else { 1 };
                let slice_bytes = u64::from(pitch) * u64::from(rows) * bpe;
                MipLevel {
                    pitch,
                    rows,
                    depth_pitch,
                    offset: 0,
                    slice_bytes,
                    bytes: slice_bytes * u64::from(depth_pitch),
                    tail: None,
                    ..Self::level_data(&d, mip)
                }
            })
            .collect();
        let mut chain_bytes = 0;
        for level in mips.iter_mut().rev() {
            level.offset = chain_bytes;
            chain_bytes += level.slice_bytes;
        }
        PlaneLayout {
            desc: d,
            block: [pitch_step, 1, 1],
            thick: false,
            stores: d.depth_or_slices,
            chain_bytes,
            bytes: chain_bytes * u64::from(d.depth_or_slices),
            alignment: 256,
            first_tail_mip: d.mips,
            mips,
            spread: None,
        }
    }

    /// The parts of a level that do not depend on the mode: its data size.
    /// The placement fields are left for the caller to fill.
    fn level_data(d: &PlaneDesc, mip: u32) -> MipLevel {
        MipLevel {
            width: data_extent(d.width, mip, d.element_texels),
            height: data_extent(d.height, mip, d.element_texels),
            depth: if d.shape == Shape::ThreeD {
                data_extent(d.depth_or_slices, mip, 1)
            } else {
                1
            },
            texel_width: (d.width >> mip).max(1),
            texel_height: (d.height >> mip).max(1),
            pitch: 0,
            rows: 0,
            depth_pitch: 1,
            offset: 0,
            slice_bytes: 0,
            bytes: 0,
            tail: None,
        }
    }

    /// A layout in blocks.
    fn tiled(d: PlaneDesc) -> PlaneLayout {
        let element_log2 = d.element_bytes.trailing_zeros();
        let block_log2 = d.mode.block_log2();
        let block_bytes = d.mode.block_bytes();
        let bpe = u64::from(d.element_bytes);
        let family = d.mode.family();
        let volume = d.shape == Shape::ThreeD;
        let thick = volume && matches!(family, Family::Standard | Family::Display);
        let addressable = !d.mode.has_xor() && d.samples == 1;

        // The interleaving, where it is known, and the block it implies.
        let thick_micro = &THICK[element_log2 as usize];
        let bits = if thick {
            Some(axis_bits(thick_micro, block_log2, true))
        } else if addressable {
            let table = if family == Family::Display {
                &DISPLAY_THIN
            } else {
                &STANDARD_THIN
            };
            Some(axis_bits(&table[element_log2 as usize], block_log2, false))
        } else {
            None
        };
        let block = match (&bits, thick) {
            (Some(bits), true) => [1 << bits[0].len(), 1 << bits[1].len(), 1 << bits[2].len()],
            _ => {
                let [w, h] = thin_block(block_log2, element_log2, d.samples.trailing_zeros());
                [w, h, 1]
            }
        };
        let [bw, bh, bd] = block;

        // The smallest unit a tail level is placed in: a 256-byte block.
        let micro_block = if thick {
            [
                1 << thick_micro.x.len(),
                1 << thick_micro.y.len(),
                1 << thick_micro.z.len(),
            ]
        } else {
            let [w, h] = thin_block(8, element_log2, 0);
            [w, h, 1]
        };

        // The largest level the tail takes: half a block, cut along the
        // axis that got the block's last address bit.
        let mut tail_share = block;
        if thick {
            match block_log2 % 3 {
                0 => tail_share[1] /= 2,
                1 => tail_share[0] /= 2,
                _ => tail_share[2] /= 2,
            }
        } else {
            tail_share[0] /= 2;
        }
        // Depth planes of 1- and 2-byte elements enter the tail as if their
        // elements were 4 bytes, so that the depth metadata lines up.
        let mut entry = [tail_share[0], tail_share[1]];
        if family == Family::Depth && element_log2 < 2 {
            let own = thin_block(8, element_log2, 0);
            let four_byte = thin_block(8, 2, 0);
            entry[0] /= own[0] / four_byte[0];
            entry[1] /= own[1] / four_byte[1];
        }
        let capacity = tail_capacity(block_log2, thick);

        let placed = |mip: u32| {
            [
                placed_extent(d.width, mip, d.element_texels),
                placed_extent(d.height, mip, d.element_texels),
                placed_extent(d.depth_or_slices, mip, 1),
            ]
        };
        // A single level never forms a tail; neither do 256-byte blocks,
        // whose capacity is zero.
        let first_tail_mip = if d.mips == 1 {
            1
        } else {
            (0..d.mips)
                .find(|&mip| {
                    let [w, h, _] = placed(mip);
                    w <= entry[0] && h <= entry[1] && d.mips - mip <= capacity
                })
                .unwrap_or(d.mips)
        };
        let has_tail = first_tail_mip < d.mips;

        let mut mips = Vec::with_capacity(d.mips as usize);
        // Bytes each standard level takes in one slice or slab.
        let mut footprints = Vec::with_capacity(first_tail_mip as usize);
        for mip in 0..first_tail_mip {
            let [w, h, depth] = placed(mip);
            let pitch = w.next_multiple_of(bw);
            let rows = h.next_multiple_of(bh);
            let depth_pitch = match (volume, thick) {
                (false, _) => 1,
                (true, true) => depth.next_multiple_of(bd),
                (true, false) => d.depth_or_slices,
            };
            let slice_bytes = u64::from(pitch) * u64::from(rows) * bpe * u64::from(d.samples);
            footprints.push(slice_bytes * u64::from(bd));
            mips.push(MipLevel {
                pitch,
                rows,
                depth_pitch,
                slice_bytes,
                bytes: slice_bytes * u64::from(depth_pitch),
                ..Self::level_data(&d, mip)
            });
        }
        // The tail block comes first, then the standard levels from the
        // smallest to the largest.
        let mut chain_bytes = if has_tail { block_bytes } else { 0 };
        for (level, footprint) in mips.iter_mut().zip(&footprints).rev() {
            level.offset = chain_bytes;
            chain_bytes += footprint;
        }

        let (mut pitch, mut rows) = (tail_share[0], tail_share[1]);
        for mip in first_tail_mip..d.mips {
            // The tail has `capacity` places; the largest level in it takes
            // the last one and each smaller level the one before.
            let place = capacity - 1 - (mip - first_tail_mip);
            let nominal: u32 = if place > 6 { 16 << place } else { place << 8 };
            // Above the first 256 bytes the address bits alternate y, x: so
            // the offset's even bits from bit 8 are a row of 256-byte blocks
            // and the odd bits a column.
            let gather =
                |from: u32| -> u32 { (0..6).map(|i| ((nominal >> (from + 2 * i)) & 1) << i).sum() };
            let place_xy = TailPlace {
                offset: nominal,
                x: gather(9) * micro_block[0],
                y: gather(8) * micro_block[1],
            };
            let depth_pitch = match (volume, thick) {
                (false, _) => 1,
                (true, true) => placed(mip)[2].next_multiple_of(micro_block[2]),
                (true, false) => d.depth_or_slices,
            };
            let slice_bytes = if thick {
                block_bytes / u64::from(bd)
            } else {
                u64::from(pitch) * u64::from(rows) * bpe
            };
            mips.push(MipLevel {
                pitch,
                rows,
                depth_pitch,
                offset: if thick { 0 } else { u64::from(nominal) },
                slice_bytes,
                bytes: slice_bytes * u64::from(depth_pitch),
                tail: Some(place_xy),
                ..Self::level_data(&d, mip)
            });
            pitch = (pitch / 2).max(micro_block[0]);
            rows = (rows / 2).max(micro_block[1]);
        }

        let stores = d.depth_or_slices.div_ceil(bd);
        let spread = bits.filter(|_| addressable).map(|bits| Spread {
            x: deposit(&bits[0]),
            y: deposit(&bits[1]),
            z: deposit(&bits[2]),
        });
        PlaneLayout {
            desc: d,
            block,
            thick,
            stores,
            chain_bytes,
            bytes: chain_bytes * u64::from(stores),
            alignment: block_bytes,
            first_tail_mip,
            mips,
            spread,
        }
    }

    /// The description the layout was built from.
    pub fn desc(&self) -> &PlaneDesc {
        &self.desc
    }

    /// The layout mode.
    pub fn mode(&self) -> Mode {
        self.desc.mode
    }

    /// Bytes in one element.
    pub fn element_bytes(&self) -> u32 {
        self.desc.element_bytes
    }

    /// Width, height and depth of one block in elements. For the row-major
    /// mode: the number of elements a row's length is a multiple of, 1, 1.
    pub fn block(&self) -> [u32; 3] {
        self.block
    }

    /// Whether blocks span several depth layers of a volume.
    pub fn is_thick(&self) -> bool {
        self.thick
    }

    /// How many units are stored one after another, each holding a whole mip
    /// chain: array slices, slabs of `block()[2]` depth layers in a thick
    /// mode, or depth layers of a volume in the other modes.
    pub fn stored_slices(&self) -> u32 {
        self.stores
    }

    /// Bytes of one stored unit: the whole mip chain of a slice or slab.
    pub fn slice_bytes(&self) -> u64 {
        self.chain_bytes
    }

    /// Bytes of the whole plane.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The alignment the plane's memory needs: the block size, or 256 for
    /// the row-major mode.
    pub fn alignment(&self) -> u64 {
        self.alignment
    }

    /// The first mip level that lies in the mip tail. Equal to the number of
    /// levels when there is no tail, so it is also the number of standard
    /// levels.
    pub fn first_tail_mip(&self) -> u32 {
        self.first_tail_mip
    }

    /// The number of levels in the mip tail.
    pub fn tail_mips(&self) -> u32 {
        self.desc.mips - self.first_tail_mip
    }

    /// Every mip level, largest first.
    pub fn mips(&self) -> &[MipLevel] {
        &self.mips
    }

    /// One mip level, or `None` past the last.
    pub fn mip(&self, mip: u32) -> Option<&MipLevel> {
        self.mips.get(mip as usize)
    }

    /// Whether element addresses and copies work for this layout: true for
    /// the row-major mode and for single-sampled textures in the modes
    /// without XOR. The module documentation says why the others are not
    /// built.
    pub fn is_addressable(&self) -> bool {
        self.desc.mode == Mode::Linear || self.spread.is_some()
    }

    /// How many depth layers one subresource of level `mip` has: the level's
    /// depth for a volume, else 1. Zero past the last level.
    pub fn subresource_depth(&self, mip: u32) -> u32 {
        self.mip(mip).map_or(0, |level| level.depth)
    }

    /// The number of slices a caller can name: array slices, or 1 for a
    /// volume, whose depth layers all belong to one subresource per level.
    pub fn subresource_slices(&self) -> u32 {
        if self.desc.shape == Shape::ThreeD {
            1
        } else {
            self.desc.depth_or_slices
        }
    }

    /// How many bytes from the start of the plane hold level `first` and
    /// every smaller level.
    ///
    /// Smaller levels lie nearer the start, so a title that streams a
    /// texture may have memory for the small levels only. This is the whole
    /// plane wherever slices follow one another (arrays, volumes), for
    /// multisampled textures, and for `first` 0 or past the last level.
    pub fn bytes_from_mip(&self, first: u32) -> u64 {
        let whole = first == 0
            || first >= self.desc.mips
            || self.stores != 1
            || self.desc.shape == Shape::ThreeD
            || self.desc.samples != 1;
        if whole {
            return self.bytes;
        }
        self.mips[first as usize..]
            .iter()
            .map(|level| match level.tail {
                Some(_) => self.alignment,
                None => level.offset + level.slice_bytes,
            })
            .max()
            .map_or(self.bytes, |end| end.min(self.bytes))
    }

    /// The byte offset, from the start of the plane, of the element at
    /// column `x` and row `y` of level `mip`. `z` is the depth layer of a
    /// volume and the array slice of any other shape. `sample` is the sample
    /// index, 0 for a texture that is not multisampled.
    ///
    /// Fails with [`Error::OutOfRange`] when the position is outside the
    /// level, and with [`Error::Unsupported`] when it is inside but the
    /// layout is not addressable (see [`PlaneLayout::is_addressable`]).
    pub fn element_offset(
        &self,
        mip: u32,
        x: u32,
        y: u32,
        z: u32,
        sample: u32,
    ) -> Result<u64, Error> {
        let level = self.mip(mip).ok_or(Error::OutOfRange)?;
        let layers = if self.desc.shape == Shape::ThreeD {
            level.depth
        } else {
            self.desc.depth_or_slices
        };
        if x >= level.width || y >= level.height || z >= layers || sample >= self.desc.samples {
            return Err(Error::OutOfRange);
        }
        if !self.is_addressable() {
            return Err(Error::Unsupported);
        }
        let (base, inner) = self.store_of(level, z);
        Ok(base + self.row_part(level, y, inner) + self.column_part(level, x))
    }

    /// The offset at which the blocks of `level` start for depth layer or
    /// slice `z`, and the layer's index inside its slab (0 in thin modes).
    pub(super) fn store_of(&self, level: &MipLevel, z: u32) -> (u64, u32) {
        let (store, inner) = if self.thick {
            (z / self.block[2], z % self.block[2])
        } else {
            (z, 0)
        };
        // A tail level is addressed inside the tail block, which starts the
        // slice; its own offset is already part of its position there.
        let start = if level.in_tail() { 0 } else { level.offset };
        (u64::from(store) * self.chain_bytes + start, inner)
    }

    /// The part of an element's address that its row, and its depth inside a
    /// slab, decide. Adding [`PlaneLayout::column_part`] gives the address
    /// inside the level, because the two parts use different address bits.
    pub(super) fn row_part(&self, level: &MipLevel, y: u32, inner: u32) -> u64 {
        let Some(spread) = &self.spread else {
            return u64::from(y) * u64::from(level.pitch) * u64::from(self.desc.element_bytes);
        };
        let [bw, bh, _] = self.block;
        let y = y + level.tail.map_or(0, |tail| tail.y);
        let blocks_before = u64::from(y / bh) * u64::from(level.pitch / bw);
        let within = spread.y[(y % bh) as usize] | spread.z[inner as usize];
        (blocks_before << self.desc.mode.block_log2()) + u64::from(within)
    }

    /// The part of an element's address that its column decides.
    pub(super) fn column_part(&self, level: &MipLevel, x: u32) -> u64 {
        let Some(spread) = &self.spread else {
            return u64::from(x) * u64::from(self.desc.element_bytes);
        };
        let bw = self.block[0];
        let x = x + level.tail.map_or(0, |tail| tail.x);
        (u64::from(x / bw) << self.desc.mode.block_log2()) + u64::from(spread.x[(x % bw) as usize])
    }
}
