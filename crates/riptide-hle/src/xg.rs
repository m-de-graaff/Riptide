//! `xg_xs.dll`: the console's texture-layout library.
//!
//! A title asks it which layout mode a texture should have, and makes a "texture computer"
//! for a description, which says how large the texture is, where each level lies, and
//! copies rows into and out of the layout. The arithmetic is `riptide-formats`'; this
//! module is the two functions and the computer object a title holds.
//!
//! Built: the suggested mode, and a computer's layout, element offsets and copies. Its
//! other methods name themselves when called. Offsets and copies work for the modes
//! `riptide-formats` can address, which leaves out the modes render targets and depth
//! buffers use.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use riptide_formats::texture::{
    self, Error, LinearPitch, Mode, PlaneLayout, PlaneRole, Shape, Texture, TextureDesc, Usage,
};
use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::trace::trace;

const MODULE: &str = "xg_xs.dll";

const S_OK: i32 = 0;
const E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

/// `D3D12_RESOURCE_DESC`. On the console its layout field holds `0x100 | mode`, or 1 for
/// row after row, or 0 for "the library's choice".
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Description {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

const _: () = assert!(size_of::<Description>() == 0x38);

/// Set in a layout field that names a mode.
const HAS_MODE: u32 = 0x100;
const LAYOUT_ROW_MAJOR: u32 = 1;
const FLAG_RENDER_TARGET: u32 = 1;
const FLAG_DEPTH_STENCIL: u32 = 2;

fn shape(dimension: u32) -> Option<Shape> {
    match dimension {
        2 => Some(Shape::OneD),
        3 => Some(Shape::TwoD),
        4 => Some(Shape::ThreeD),
        _ => None,
    }
}

fn usage(flags: u32) -> Usage {
    if flags & FLAG_DEPTH_STENCIL != 0 {
        Usage::DepthStencil
    } else if flags & FLAG_RENDER_TARGET != 0 {
        Usage::RenderTarget
    } else {
        Usage::Sampled
    }
}

/// The mode a description's layout field asks for: the one it names, row after row, or
/// with no choice made the suggested one.
fn mode(description: &Description) -> Option<Mode> {
    let layout = description.layout;
    if layout & HAS_MODE != 0 {
        Mode::from_number(layout & 0xff)
    } else if layout == LAYOUT_ROW_MAJOR {
        Some(Mode::Linear)
    } else {
        Some(texture::suggested_mode(
            shape(description.dimension)?,
            usage(description.flags),
        ))
    }
}

/// The layout of the texture a description describes.
fn laid_out(description: &Description) -> Option<Texture> {
    let desc = TextureDesc {
        shape: shape(description.dimension)?,
        width: u32::try_from(description.width).ok()?,
        height: description.height,
        depth_or_slices: u32::from(description.depth_or_array_size).max(1),
        mips: u32::from(description.mip_levels),
        format: description.format,
        samples: description.sample_count,
        mode: mode(description)?,
    };
    Texture::new(&desc).ok()
}

/// `XGSuggestSwizzleMode`: `(description, flags, layout out)`. The title puts the answer in
/// the description's layout field.
///
/// # Safety
/// `description` is null or valid; `out` is null or where the answer goes.
unsafe extern "system" fn suggest_swizzle_mode(
    description: *const Description,
    _flags: u32,
    out: *mut u32,
) -> i32 {
    // SAFETY: the title's pointers; null is refused.
    let (Some(description), Some(out)) = (unsafe { (description.as_ref(), out.as_mut()) }) else {
        return E_INVALIDARG;
    };
    let Some(shape) = shape(description.dimension) else {
        return E_INVALIDARG;
    };
    *out = HAS_MODE | texture::suggested_mode(shape, usage(description.flags)).number();
    S_OK
}

/// A texture computer as a title holds it: a table of methods, then what is ours.
#[repr(C)]
pub(crate) struct Computer {
    table: *const usize,
    references: AtomicU32,
    description: Description,
    texture: Texture,
}

const SLOTS: usize = 13;

/// What the computer's methods are called here, by position. The library's own names for
/// the unbuilt ones are not known; these say what each is believed to answer.
const NAMES: [&str; SLOTS] = [
    "AddRef",
    "Release",
    "layout",
    "size",
    "alignment",
    "level offset",
    "element offset",
    "element coordinate",
    "copy into a subresource",
    "copy out of a subresource",
    "64 KiB tiling",
    "view descriptor",
    "tiled resource query",
];

fn table() -> &'static [usize; SLOTS] {
    static TABLE: OnceLock<[usize; SLOTS]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let built: [(usize, usize); 6] = [
            (0, add_ref as *const () as usize),
            (1, release as *const () as usize),
            (2, layout as *const () as usize),
            (6, element_offset as *const () as usize),
            (8, copy_into as *const () as usize),
            (9, copy_from as *const () as usize),
        ];
        std::array::from_fn(|slot| {
            built.iter().find(|&&(at, _)| at == slot).map_or_else(
                || {
                    crate::com::stub(format!(
                        "XGTextureComputer {} (slot {slot}, +{:#x})",
                        NAMES[slot],
                        slot * 8
                    ))
                },
                |&(_, method)| method,
            )
        })
    })
}

/// `XGCreateTextureComputer`: `(description, computer out)`.
///
/// # Safety
/// `description` is null or valid; `out` is null or where the computer goes.
unsafe extern "system" fn create_texture_computer(
    description: *const Description,
    out: *mut *mut Computer,
) -> i32 {
    // SAFETY: the title's description; null is refused.
    let Some(&description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let Some(texture) = laid_out(&description) else {
        trace!("XGCreateTextureComputer: no layout for {description:?}");
        // SAFETY: checked non-null. A title that does not look at the result must not be
        // left with a pointer of its own making.
        unsafe { out.write(std::ptr::null_mut()) };
        return E_INVALIDARG;
    };
    show(&description, &texture);
    let computer = Box::new(Computer {
        table: table().as_ptr(),
        references: AtomicU32::new(1),
        description,
        texture,
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(computer)) };
    S_OK
}

/// Says what was laid out, for the first few: a title makes a computer per texture.
fn show(description: &Description, texture: &Texture) {
    static SHOWN: AtomicU32 = AtomicU32::new(0);
    if SHOWN.fetch_add(1, Ordering::Relaxed) < 8 {
        trace!(
            "texture computer: {} x {} x {}, {} levels, format {}, layout {:#x}: {:#x} bytes \
             aligned to {:#x}",
            description.width,
            description.height,
            description.depth_or_array_size,
            texture.mips(),
            description.format,
            description.layout,
            texture.bytes(),
            texture.alignment()
        );
    }
}

extern "system" fn add_ref(this: &Computer) -> u32 {
    this.references.fetch_add(1, Ordering::Relaxed) + 1
}

/// # Safety
/// `this` came from [`create_texture_computer`] and the caller gives up one reference.
unsafe extern "system" fn release(this: *mut Computer) -> u32 {
    // SAFETY: a live computer.
    let left = unsafe { &*this }.references.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: that was the last reference, and the computer came from `Box::into_raw`.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

/// The most levels a layout has room for.
const LEVELS: usize = 15;

/// One level in the layout a title reads.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct LevelOut {
    bytes: u64,
    offset: u64,
    slice_bytes: u64,
    pitch_texels: u32,
    pitch_bytes: u32,
    alignment: u32,
    /// Padded width and height in elements, then padded depth or the array size.
    padded: [u32; 3],
    elements: [u32; 2],
    texels: [u32; 2],
    depth_or_array: u32,
    samples: u32,
    mode: u32,
    bank_rotation: [u64; 2],
    block_depth: u32,
    in_tail: u32,
    /// Where a tail level of a volume starts, in elements: x in bits 0 to 10, y above.
    tail_position: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PlaneOut {
    /// 1 data, 7 depth, 8 stencil.
    usage: u32,
    bytes: u64,
    slice_bytes: u64,
    offset: u64,
    alignment: u64,
    element_bytes: u32,
    standard_levels: u32,
    tail_levels: u32,
    levels: [LevelOut; LEVELS],
}

#[repr(C)]
struct LayoutOut {
    bytes: u64,
    alignment: u64,
    levels: u32,
    planes: u32,
    plane: [PlaneOut; 4],
    dimension: u32,
}

const _: () = assert!(size_of::<LevelOut>() == 0x70);
const _: () = assert!(size_of::<PlaneOut>() == 0x6c8);
const _: () = assert!(size_of::<LayoutOut>() == 0x1b40);

fn level_out(description: &Description, plane: &PlaneLayout, level: usize) -> LevelOut {
    let one = &plane.mips()[level];
    let desc = plane.desc();
    let volume = desc.shape == Shape::ThreeD;
    let array = u32::from(description.depth_or_array_size).max(1);
    let mut out = LevelOut {
        bytes: one.bytes,
        slice_bytes: one.slice_bytes,
        pitch_texels: one.pitch * desc.element_texels,
        pitch_bytes: one.pitch * desc.element_bytes,
        padded: [
            one.pitch,
            one.rows,
            if volume { one.depth_pitch } else { array },
        ],
        elements: [one.width, one.height],
        texels: [one.texel_width, one.texel_height],
        depth_or_array: if volume { one.depth } else { array },
        samples: desc.samples,
        block_depth: plane.block()[2],
        ..LevelOut::default()
    };
    match one.tail {
        None => {
            out.offset = one.offset;
            out.alignment = plane.alignment() as u32;
            out.mode = plane.mode().number();
        }
        Some(place) => {
            out.in_tail = 1;
            out.alignment = 256;
            out.mode = plane.mode().tail_number();
            if plane.is_thick() {
                // A tail level of a volume is found by position, not by offset.
                out.tail_position = place.x | (place.y << 11);
            } else {
                out.offset = u64::from(place.offset);
            }
        }
    }
    out
}

/// The computer's layout method: `(layout out)`.
///
/// # Safety
/// `out` is null or room for a layout.
unsafe extern "system" fn layout(this: &Computer, out: *mut LayoutOut) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes room for the layout; every field is plain numbers, for
    // which zero is a value.
    let out = unsafe {
        out.write_bytes(0, 1);
        &mut *out
    };
    let texture = &this.texture;
    out.bytes = texture.bytes();
    out.alignment = texture.alignment();
    out.levels = texture.mips();
    out.planes = texture.planes().len() as u32;
    out.dimension = this.description.dimension;
    // The library calls a plane "depth" only beside stencil or without levels below it.
    let depth = this.description.flags & FLAG_DEPTH_STENCIL != 0
        && (texture.planes().len() > 1 || texture.mips() == 1);
    for (to, plane) in out.plane.iter_mut().zip(texture.planes()) {
        let one = &plane.layout;
        to.usage = match (plane.role, depth) {
            (PlaneRole::Stencil, _) => 8,
            (PlaneRole::Main, true) => 7,
            (PlaneRole::Main, false) => 1,
        };
        to.bytes = one.bytes();
        to.slice_bytes = one.slice_bytes();
        to.offset = plane.offset;
        to.alignment = one.alignment();
        to.element_bytes = one.element_bytes();
        to.standard_levels = one.first_tail_mip();
        to.tail_levels = one.tail_mips();
        for (level, slot) in to.levels.iter_mut().take(one.mips().len()).enumerate() {
            *slot = level_out(&this.description, one, level);
        }
    }
    S_OK
}

/// The computer's element offset: `(plane, level, x, y, z or slice, sample, bit position
/// out)`, returning the element's byte offset from the texture's start. A title reads
/// texels through it without looking for an error, so a position outside the texture gives
/// offset 0, which is inside it.
///
/// # Safety
/// `bit` is null or where the bit position goes.
unsafe extern "system" fn element_offset(
    this: &Computer,
    plane: u32,
    level: u32,
    x: u64,
    y: u32,
    z: u32,
    sample: u32,
    bit: *mut u32,
) -> u64 {
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(bit) = unsafe { bit.as_mut() } {
        *bit = 0;
    }
    let x = u32::try_from(x).unwrap_or(u32::MAX);
    match this.texture.element_offset(plane, level, x, y, z, sample) {
        Ok(offset) => offset,
        Err(error) => {
            trace!(
                "texture computer: no offset for plane {plane} level {level} at ({x}, {y}, \
                 {z}): {error:?}"
            );
            0
        }
    }
}

fn code(result: Result<(), Error>) -> i32 {
    match result {
        Ok(()) => S_OK,
        Err(Error::Unsupported) => E_NOTIMPL,
        Err(_) => E_INVALIDARG,
    }
}

/// What a copy of one subresource touches: the level and slice, how many bytes of the
/// texture's memory from its start, and how many bytes of the plain rows.
fn copy_extent(
    this: &Computer,
    plane: u32,
    subresource: u32,
    pitch: LinearPitch,
) -> Result<(u32, u32, usize, usize), Error> {
    let (level, slice) = this
        .texture
        .subresource(subresource)
        .ok_or(Error::OutOfRange)?;
    let tiled = usize::try_from(this.texture.bytes()).map_err(|_| Error::TooSmall)?;
    let linear = this
        .texture
        .plane(plane)?
        .layout
        .linear_bytes(level, pitch)?;
    Ok((level, slice, tiled, linear))
}

/// The computer's copy into the layout: `(texture memory, plane, subresource, plain rows,
/// row pitch, depth pitch)`.
///
/// # Safety
/// `tiled` is null or the whole texture's memory; `linear` is null or the subresource's
/// rows at those pitches.
unsafe extern "system" fn copy_into(
    this: &Computer,
    tiled: *mut u8,
    plane: u32,
    subresource: u32,
    linear: *const u8,
    row: u32,
    depth: u32,
) -> i32 {
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let pitch = LinearPitch {
        row: row as usize,
        depth: depth as usize,
    };
    let result = copy_extent(this, plane, subresource, pitch).and_then(
        |(level, slice, tiled_bytes, linear_bytes)| {
            // SAFETY: the title's memory holds the whole texture and those rows.
            let (tiled, linear) = unsafe {
                (
                    std::slice::from_raw_parts_mut(tiled, tiled_bytes),
                    std::slice::from_raw_parts(linear, linear_bytes),
                )
            };
            this.texture.tile(tiled, plane, level, slice, linear, pitch)
        },
    );
    if let Err(error) = result {
        trace!(
            "texture computer: copy into subresource {subresource} of {:?} failed: {error:?}",
            this.description
        );
    }
    code(result)
}

/// The computer's copy out of the layout: `(plain rows, plane, subresource, sample,
/// texture memory, row pitch, depth pitch)`.
///
/// # Safety
/// As [`copy_into`], with the rows written and the texture read.
#[allow(clippy::too_many_arguments)] // the method's own arguments
unsafe extern "system" fn copy_from(
    this: &Computer,
    linear: *mut u8,
    plane: u32,
    subresource: u32,
    _sample: u32,
    tiled: *const u8,
    row: u32,
    depth: u32,
) -> i32 {
    if tiled.is_null() || linear.is_null() {
        return E_INVALIDARG;
    }
    let pitch = LinearPitch {
        row: row as usize,
        depth: depth as usize,
    };
    let result = copy_extent(this, plane, subresource, pitch).and_then(
        |(level, slice, tiled_bytes, linear_bytes)| {
            // SAFETY: the title's memory holds the whole texture and room for those rows.
            let (tiled, linear) = unsafe {
                (
                    std::slice::from_raw_parts(tiled, tiled_bytes),
                    std::slice::from_raw_parts_mut(linear, linear_bytes),
                )
            };
            this.texture
                .detile(tiled, plane, level, slice, linear, pitch)
        },
    );
    if let Err(error) = result {
        trace!(
            "texture computer: copy out of subresource {subresource} of {:?} failed: {error:?}",
            this.description
        );
    }
    code(result)
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, usize); 2] = [
        (
            "XGSuggestSwizzleMode",
            suggest_swizzle_mode as *const () as usize,
        ),
        (
            "XGCreateTextureComputer",
            create_texture_computer as *const () as usize,
        ),
    ];
    for (name, address) in functions {
        // SAFETY: each function has the arguments of the console function it is bound as.
        unsafe { imports.implement(MODULE, Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn described(width: u64, height: u32, levels: u16, format: u32, layout: u32) -> Description {
        Description {
            dimension: 3,
            alignment: 0,
            width,
            height,
            depth_or_array_size: 1,
            mip_levels: levels,
            format,
            sample_count: 1,
            sample_quality: 0,
            layout,
            flags: 0,
        }
    }

    #[test]
    fn the_suggested_mode_goes_by_what_the_texture_is_for() {
        let mut out = 0;
        let sampled = described(256, 128, 1, 61, 0);
        let target = Description {
            flags: FLAG_RENDER_TARGET,
            ..sampled
        };
        // SAFETY: valid descriptions and out-pointer, and the null ones under test.
        unsafe {
            assert_eq!(suggest_swizzle_mode(&sampled, 0, &mut out), S_OK);
            assert_eq!(out, HAS_MODE | Mode::Standard4K.number());
            assert_eq!(suggest_swizzle_mode(&target, 0, &mut out), S_OK);
            assert_eq!(out, HAS_MODE | Mode::Render64KX.number());
            assert_eq!(
                suggest_swizzle_mode(std::ptr::null(), 0, &mut out),
                E_INVALIDARG
            );
            assert_eq!(
                suggest_swizzle_mode(&sampled, 0, std::ptr::null_mut()),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn a_computer_lays_a_texture_out_and_copies_rows_in_and_out() {
        let layout_field = HAS_MODE | Mode::Standard4K.number();
        let description = described(64, 32, 0, 28, layout_field);
        let mut computer = std::ptr::null_mut();
        // SAFETY: a valid description; the computer is used as made and released once.
        unsafe {
            assert_eq!(create_texture_computer(&description, &mut computer), S_OK);
            let this = &*computer;
            let mut out = Box::new(std::mem::zeroed::<LayoutOut>());
            assert_eq!(layout(this, &raw mut *out), S_OK);
            assert_eq!((out.planes, out.levels, out.dimension), (1, 7, 3));
            assert_eq!(out.bytes, this.texture.bytes());
            let plane = &out.plane[0];
            assert_eq!((plane.usage, plane.element_bytes), (1, 4));
            assert_eq!(plane.standard_levels + plane.tail_levels, 7);
            assert_eq!(plane.levels[0].texels, [64, 32]);
            assert_eq!(plane.levels[0].mode, Mode::Standard4K.number());
            assert_eq!(plane.levels[6].in_tail, 1);

            // Rows with a pitch wider than the row go in and come back the same.
            let (row, rows) = (64 * 4 + 16, 32);
            let source: Vec<u8> = (0..row * rows).map(|byte| byte as u8).collect();
            let mut memory = vec![0u8; out.bytes as usize];
            let mut back = vec![0u8; row * rows];
            assert_eq!(
                copy_into(
                    this,
                    memory.as_mut_ptr(),
                    0,
                    0,
                    source.as_ptr(),
                    row as u32,
                    0
                ),
                S_OK
            );
            assert_eq!(
                copy_from(
                    this,
                    back.as_mut_ptr(),
                    0,
                    0,
                    0,
                    memory.as_ptr(),
                    row as u32,
                    0
                ),
                S_OK
            );
            for line in 0..rows {
                let at = line * row;
                assert_eq!(back[at..at + 256], source[at..at + 256]);
            }

            // The element at (5, 3) is where its bytes were put.
            let mut bit = 9;
            let offset = element_offset(this, 0, 0, 5, 3, 0, 0, &mut bit) as usize;
            assert_eq!(bit, 0);
            assert_eq!(
                memory[offset..offset + 4],
                source[3 * row + 20..3 * row + 24]
            );
            // Outside the texture: offset 0, not an error value.
            assert_eq!(
                element_offset(this, 0, 0, 500, 3, 0, 0, std::ptr::null_mut()),
                0
            );

            assert_eq!(add_ref(this), 2);
            assert_eq!(release(computer), 1);
            assert_eq!(release(computer), 0);
        }
    }

    #[test]
    fn a_description_with_no_layout_makes_no_computer() {
        let unknown = described(64, 32, 1, 250, 0);
        let mut computer: *mut Computer = std::ptr::dangling_mut();
        // SAFETY: a valid description and out-pointer, and the null ones under test.
        unsafe {
            assert_eq!(
                create_texture_computer(&unknown, &mut computer),
                E_INVALIDARG
            );
            assert!(computer.is_null());
            assert_eq!(
                create_texture_computer(std::ptr::null(), &mut computer),
                E_INVALIDARG
            );
        }
    }
}
