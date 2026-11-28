//! Whole textures: a format number becomes one or two planes, placed one
//! after the other, and the mode the console's library suggests.

use super::{Error, LinearPitch, Mode, PlaneDesc, PlaneLayout, Shape};

/// What a pixel format stores per element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FormatInfo {
    /// Bytes in one element of the first plane.
    pub element_bytes: u32,
    /// Texels along one side of an element: 1, or 4 for block compression.
    pub element_texels: u32,
    /// Whether the format has stencil, which the GPU keeps in a second plane
    /// of one byte per texel.
    pub stencil_plane: bool,
}

/// What the format with DXGI number `format` stores, including the numbers
/// the console adds to the desktop list (116 and 117, its two 10-bit
/// floating-point formats; 118 to 120, 16-bit depth with stencil).
///
/// `None` for the unknown format and for formats with no layout here: the
/// 96-bit formats, the 1-bit format, the packed 4:2:2 formats and the video
/// formats. Nothing is known to be wrong with those; no title has needed
/// them.
pub fn format_info(format: u32) -> Option<FormatInfo> {
    let plain = |element_bytes| FormatInfo {
        element_bytes,
        element_texels: 1,
        stencil_plane: false,
    };
    let with_stencil = |element_bytes| FormatInfo {
        stencil_plane: true,
        ..plain(element_bytes)
    };
    let compressed = |element_bytes| FormatInfo {
        element_texels: 4,
        ..plain(element_bytes)
    };
    Some(match format {
        // Four 32-bit channels.
        1..=4 => plain(16),
        // Four 16-bit or two 32-bit channels.
        9..=18 => plain(8),
        // 32-bit depth with stencil, 24-bit depth with stencil.
        19..=22 | 44..=47 => with_stencil(4),
        // The 32-bit colour, two 16-bit and one 32-bit channel formats,
        // shared-exponent, the BGRA formats, and the console's 10-bit floats.
        23..=43 | 67 | 87..=93 | 116 | 117 => plain(4),
        // Two 8-bit or one 16-bit channel, and the packed 16-bit colours.
        48..=59 | 85 | 86 | 115 => plain(2),
        // The console's 16-bit depth with stencil.
        118..=120 => with_stencil(2),
        // One 8-bit channel.
        60..=65 => plain(1),
        // BC1 and BC4: 8 bytes per 4x4 block.
        70..=72 | 79..=81 => compressed(8),
        // BC2, BC3, BC5, BC6H and BC7: 16 bytes per 4x4 block.
        73..=78 | 82..=84 | 94..=99 => compressed(16),
        _ => return None,
    })
}

/// What a texture is for, as far as the choice of a mode cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Usage {
    /// A texture that is only sampled.
    Sampled,
    /// A texture the GPU renders colour into.
    RenderTarget,
    /// A depth or depth-stencil buffer. A description that allows both this
    /// and rendering colour counts as this.
    DepthStencil,
}

/// The mode the console's library suggests for a texture whose description
/// names none: row-major for one-dimensional textures whatever their use,
/// the 64 KiB depth mode for depth buffers, the 64 KiB render mode for
/// render targets, and the standard 4 KiB mode for everything else, of any
/// size, volumes and cubes included.
///
/// This is the rule of an older function of the library that picks the best
/// mode. The function titles call today is newer than every copy that rule
/// could be checked against, so treat the rule as likely rather than
/// certain. If textures that carry no mode of their own come out scrambled,
/// look here first.
pub fn suggested_mode(shape: Shape, usage: Usage) -> Mode {
    match (shape, usage) {
        (Shape::OneD, _) => Mode::Linear,
        (_, Usage::DepthStencil) => Mode::Depth64KX,
        (_, Usage::RenderTarget) => Mode::Render64KX,
        (_, Usage::Sampled) => Mode::Standard4K,
    }
}

/// A texture as a title describes it to the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureDesc {
    /// The shape of the texture.
    pub shape: Shape,
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Depth of a volume; for the other shapes the number of array slices
    /// (six for a cube).
    pub depth_or_slices: u32,
    /// Number of mip levels; 0 asks for the full chain down to one texel.
    pub mips: u32,
    /// DXGI format number.
    pub format: u32,
    /// Samples per texel; 0 counts as 1.
    pub samples: u32,
    /// The layout mode.
    pub mode: Mode,
}

/// Which part of the texel a plane holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaneRole {
    /// The colour of a colour format, or the depth of a depth format.
    Main,
    /// The stencil of a depth-stencil format.
    Stencil,
}

/// One plane of a texture and where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// Byte offset of the plane from the start of the texture.
    pub offset: u64,
    /// What the plane holds.
    pub role: PlaneRole,
    /// The plane's layout. Its offsets are from the start of the plane.
    pub layout: PlaneLayout,
}

/// The layout of a whole texture: its planes, one after the other.
///
/// Only the planes that hold texel data are laid out. The console also adds
/// compression metadata to render targets and depth buffers unless the
/// description forbids it, so [`Texture::bytes`] is smaller than the
/// console's answer for those. A host renders into its own targets, so the
/// difference only matters to a title that sizes a memory pool from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    planes: Vec<Plane>,
    mips: u32,
    bytes: u64,
    alignment: u64,
}

impl Texture {
    /// Lays out the texture `desc` describes.
    ///
    /// Fails with [`Error::Invalid`] for a format [`format_info`] does not
    /// know and for every description [`PlaneLayout::new`] refuses.
    pub fn new(desc: &TextureDesc) -> Result<Texture, Error> {
        let format = format_info(desc.format).ok_or(Error::Invalid)?;
        let mips = match desc.mips {
            0 => {
                let depth = if desc.shape == Shape::ThreeD {
                    desc.depth_or_slices
                } else {
                    1
                };
                desc.width.max(desc.height).max(depth).max(1).ilog2() + 1
            }
            given => given,
        };
        let plane = |element_bytes| {
            PlaneLayout::new(&PlaneDesc {
                shape: desc.shape,
                width: desc.width,
                height: desc.height,
                depth_or_slices: desc.depth_or_slices,
                mips,
                element_bytes,
                element_texels: format.element_texels,
                samples: desc.samples.max(1),
                mode: desc.mode,
            })
        };
        let mut planes = vec![Plane {
            offset: 0,
            role: PlaneRole::Main,
            layout: plane(format.element_bytes)?,
        }];
        let mut bytes = planes[0].layout.bytes();
        let mut alignment = planes[0].layout.alignment();
        if format.stencil_plane {
            let layout = plane(1)?;
            let offset = bytes.next_multiple_of(layout.alignment());
            bytes = offset + layout.bytes();
            alignment = alignment.max(layout.alignment());
            planes.push(Plane {
                offset,
                role: PlaneRole::Stencil,
                layout,
            });
        }
        Ok(Texture {
            planes,
            mips,
            bytes,
            alignment,
        })
    }

    /// The planes, in the order they lie in memory.
    pub fn planes(&self) -> &[Plane] {
        &self.planes
    }

    /// One plane, or [`Error::OutOfRange`] past the last.
    pub fn plane(&self, plane: u32) -> Result<&Plane, Error> {
        self.planes.get(plane as usize).ok_or(Error::OutOfRange)
    }

    /// The number of mip levels, with a full chain counted out.
    pub fn mips(&self) -> u32 {
        self.mips
    }

    /// Bytes of the whole texture.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The alignment the texture's memory needs: the largest of its planes'.
    pub fn alignment(&self) -> u64 {
        self.alignment
    }

    /// The mip level and array slice of subresource `index`, as the console
    /// numbers them inside one plane: levels of slice 0 first, then those of
    /// slice 1. `None` past the last. A volume has one slice.
    pub fn subresource(&self, index: u32) -> Option<(u32, u32)> {
        let (mip, slice) = (index % self.mips, index / self.mips);
        (slice < self.planes[0].layout.subresource_slices()).then_some((mip, slice))
    }

    /// The byte offset of one element from the start of the texture: see
    /// [`PlaneLayout::element_offset`], to which this adds the plane's own
    /// offset.
    pub fn element_offset(
        &self,
        plane: u32,
        mip: u32,
        x: u32,
        y: u32,
        z: u32,
        sample: u32,
    ) -> Result<u64, Error> {
        let plane = self.plane(plane)?;
        Ok(plane.offset + plane.layout.element_offset(mip, x, y, z, sample)?)
    }

    /// How many bytes from the start of the texture a copy of level `mip` of
    /// `plane` reads or writes at most: the plane's offset plus its
    /// [`PlaneLayout::bytes_from_mip`].
    pub fn bytes_for_copy(&self, plane: u32, mip: u32) -> Result<u64, Error> {
        let plane = self.plane(plane)?;
        Ok(plane.offset + plane.layout.bytes_from_mip(mip))
    }

    /// Copies one subresource of one plane from plain rows into the console
    /// layout. `tiled` is the texture's memory from its start, holding at
    /// least [`Texture::bytes_for_copy`]; the rest is as
    /// [`PlaneLayout::tile`].
    pub fn tile(
        &self,
        tiled: &mut [u8],
        plane: u32,
        mip: u32,
        slice: u32,
        linear: &[u8],
        pitch: LinearPitch,
    ) -> Result<(), Error> {
        let plane = self.plane(plane)?;
        let start = usize::try_from(plane.offset).map_err(|_| Error::TooSmall)?;
        let tiled = tiled.get_mut(start..).ok_or(Error::TooSmall)?;
        plane.layout.tile(tiled, mip, slice, linear, pitch)
    }

    /// Copies one subresource of one plane out of the console layout into
    /// plain rows: the reverse of [`Texture::tile`].
    pub fn detile(
        &self,
        tiled: &[u8],
        plane: u32,
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        pitch: LinearPitch,
    ) -> Result<(), Error> {
        let plane = self.plane(plane)?;
        let start = usize::try_from(plane.offset).map_err(|_| Error::TooSmall)?;
        let tiled = tiled.get(start..).ok_or(Error::TooSmall)?;
        plane.layout.detile(tiled, mip, slice, linear, pitch)
    }
}
