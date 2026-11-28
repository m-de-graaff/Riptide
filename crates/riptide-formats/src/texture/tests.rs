//! Tests of the texture layouts, on synthetic input only.
//!
//! Two kinds of test keep each other honest. The self-consistency tests walk
//! every element of a spread of textures in every addressable mode and check
//! that addresses stay inside the texture, never overlap, and that copies in
//! and out give the input back. The fixed-value tests compare sizes, tail
//! placement and the address of every element against numbers AMD's address
//! library produced, so that a wrong interleaving pattern that happens to be
//! self-consistent is still caught.

use super::*;

/// The modes whose elements can be addressed.
const ADDRESSABLE: [Mode; 7] = [
    Mode::Linear,
    Mode::Standard256,
    Mode::Display256,
    Mode::Standard4K,
    Mode::Display4K,
    Mode::Standard64K,
    Mode::Display64K,
];

/// The modes whose sizes are known but whose addresses are not built.
const XOR: [Mode; 9] = [
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

fn plane(
    shape: Shape,
    extent: [u32; 3],
    mips: u32,
    bytes: u32,
    texels: u32,
    mode: Mode,
) -> PlaneDesc {
    PlaneDesc {
        shape,
        width: extent[0],
        height: extent[1],
        depth_or_slices: extent[2],
        mips,
        element_bytes: bytes,
        element_texels: texels,
        samples: 1,
        mode,
    }
}

fn flat(width: u32, height: u32, mips: u32, bytes: u32, mode: Mode) -> PlaneDesc {
    plane(Shape::TwoD, [width, height, 1], mips, bytes, 1, mode)
}

fn layout(desc: PlaneDesc) -> PlaneLayout {
    PlaneLayout::new(&desc).unwrap_or_else(|error| panic!("{desc:?}: {error}"))
}

/// The number of levels of a full mip chain.
fn full_chain(shape: Shape, extent: [u32; 3]) -> u32 {
    let depth = if shape == Shape::ThreeD { extent[2] } else { 1 };
    extent[0].max(extent[1]).max(depth).ilog2() + 1
}

/// Bytes that look random, different for every seed.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Tiles every subresource of `desc` from padded rows, checks every element
/// address against the tiled bytes, that no two elements share a byte and
/// that none lies outside the plane, then detiles everything and compares.
fn check_plane(desc: PlaneDesc) {
    let l = layout(desc);
    assert!(l.is_addressable(), "{desc:?}");
    let total = l.bytes() as usize;
    let n = desc.element_bytes as usize;
    let volume = desc.shape == Shape::ThreeD;
    let mut owned = vec![false; total];
    let mut tiled = vec![0xee_u8; total];
    let mut inputs = Vec::new();
    for slice in 0..l.subresource_slices() {
        for mip in 0..desc.mips {
            let level = l.mips()[mip as usize];
            let row_bytes = level.width as usize * n;
            let pitch = LinearPitch {
                row: row_bytes + 5,
                depth: (row_bytes + 5) * level.height as usize + 3,
            };
            let len = l.linear_bytes(mip, pitch).unwrap();
            let linear = noise(len, u64::from(mip) * 64 + u64::from(slice) + 1);
            l.tile(&mut tiled, mip, slice, &linear, pitch).unwrap();
            assert_eq!(l.subresource_depth(mip), level.depth);
            for z in 0..level.depth {
                for y in 0..level.height {
                    for x in 0..level.width {
                        let layer = if volume { z } else { slice };
                        let at = l.element_offset(mip, x, y, layer, 0).unwrap() as usize;
                        let what = format_args!("{desc:?} mip {mip} slice {slice} ({x},{y},{z})");
                        assert!(at + n <= total, "{what} lies outside");
                        assert_eq!(at % n, 0, "{what} is misaligned");
                        assert!(!owned[at..at + n].contains(&true), "{what} shares bytes");
                        owned[at..at + n].fill(true);
                        let from =
                            z as usize * pitch.depth + y as usize * pitch.row + x as usize * n;
                        assert_eq!(tiled[at..at + n], linear[from..from + n], "{what}");
                    }
                }
            }
            inputs.push((mip, slice, pitch, linear));
        }
    }
    // Bytes that belong to no element were never written.
    for (at, &byte) in tiled.iter().enumerate() {
        assert!(owned[at] || byte == 0xee, "{desc:?}: stray write at {at}");
    }
    // Everything is still there after all subresources were written.
    for (mip, slice, pitch, linear) in inputs {
        let level = l.mips()[mip as usize];
        let row_bytes = level.width as usize * n;
        let mut back = vec![0xcc_u8; linear.len()];
        l.detile(&tiled, mip, slice, &mut back, pitch).unwrap();
        for z in 0..level.depth as usize {
            for y in 0..level.height as usize {
                let row = z * pitch.depth + y * pitch.row;
                assert_eq!(
                    back[row..row + row_bytes],
                    linear[row..row + row_bytes],
                    "{desc:?} mip {mip} slice {slice} row {y} layer {z}"
                );
            }
        }
        // The padding between rows and layers was left alone.
        let mut expected = vec![0xcc_u8; linear.len()];
        for z in 0..level.depth as usize {
            for y in 0..level.height as usize {
                let row = z * pitch.depth + y * pitch.row;
                expected[row..row + row_bytes].copy_from_slice(&linear[row..row + row_bytes]);
            }
        }
        assert_eq!(back, expected, "{desc:?} mip {mip} slice {slice} padding");
    }
}

#[test]
fn flat_textures_arrays_and_cubes_round_trip_in_every_addressable_mode() {
    // Width, height, array slices: 1x1, odd, non-square, an array, a cube,
    // one wider than a 64 KiB block of 4-byte elements, and a tall strip.
    let shapes = [
        [1, 1, 1],
        [5, 3, 1],
        [33, 17, 1],
        [100, 60, 3],
        [64, 64, 6],
        [130, 70, 1],
        [1, 37, 2],
    ];
    for mode in ADDRESSABLE {
        for bytes in [1, 2, 4, 8, 16] {
            for extent in shapes {
                let mips = full_chain(Shape::TwoD, extent);
                check_plane(plane(Shape::TwoD, extent, mips, bytes, 1, mode));
            }
            // A single level, and a chain cut short.
            check_plane(plane(Shape::TwoD, [77, 45, 1], 1, bytes, 1, mode));
            check_plane(plane(Shape::TwoD, [77, 45, 2], 3, bytes, 1, mode));
        }
    }
}

#[test]
fn block_compressed_textures_round_trip_in_every_addressable_mode() {
    for mode in ADDRESSABLE {
        for bytes in [8, 16] {
            for extent in [
                [109, 71, 1],
                [256, 256, 1],
                [4, 4, 1],
                [1, 1, 1],
                [260, 36, 6],
            ] {
                let mips = full_chain(Shape::TwoD, extent);
                check_plane(plane(Shape::TwoD, extent, mips, bytes, 4, mode));
            }
        }
    }
}

#[test]
fn rows_round_trip_in_every_addressable_mode() {
    for mode in ADDRESSABLE {
        for bytes in [1, 4, 16] {
            check_plane(plane(Shape::OneD, [1000, 1, 1], 10, bytes, 1, mode));
            check_plane(plane(Shape::OneD, [77, 1, 3], 1, bytes, 1, mode));
        }
    }
}

#[test]
fn volumes_round_trip_in_every_mode_that_addresses_them() {
    for mode in [Mode::Linear, Mode::Standard4K, Mode::Standard64K] {
        for bytes in [1, 2, 4, 8, 16] {
            for extent in [
                [9, 7, 5],
                [16, 16, 20],
                [33, 17, 10],
                [4, 4, 16],
                [1, 1, 1],
                [1, 1, 17],
            ] {
                let mips = full_chain(Shape::ThreeD, extent);
                check_plane(plane(Shape::ThreeD, extent, mips, bytes, 1, mode));
                check_plane(plane(Shape::ThreeD, extent, 1, bytes, 1, mode));
            }
        }
        check_plane(plane(Shape::ThreeD, [40, 24, 12], 4, 8, 4, mode));
    }
}

#[test]
fn copies_accept_rows_with_no_padding() {
    for mode in ADDRESSABLE {
        let l = layout(flat(73, 39, 7, 4, mode));
        let mut tiled = vec![0; l.bytes() as usize];
        for mip in 0..7 {
            let level = l.mips()[mip as usize];
            let pitch = LinearPitch::packed(level.width as usize * 4, level.height as usize);
            let rows = noise(pitch.depth, u64::from(mip) + 9);
            assert_eq!(l.linear_bytes(mip, pitch), Ok(rows.len()));
            l.tile(&mut tiled, mip, 0, &rows, pitch).unwrap();
            let mut back = vec![0; rows.len()];
            l.detile(&tiled, mip, 0, &mut back, pitch).unwrap();
            assert_eq!(back, rows, "{mode:?} mip {mip}");
        }
    }
}

/// A block holds exactly its size in elements, for every element size.
#[test]
fn blocks_hold_their_size_in_elements() {
    for mode in Mode::ALL {
        if mode == Mode::Linear {
            continue;
        }
        for bytes in [1, 2, 4, 8, 16] {
            let l = layout(flat(1, 1, 1, bytes, mode));
            let [w, h, d] = l.block();
            assert_eq!(
                u64::from(w * h * d * bytes),
                mode.block_bytes(),
                "{mode:?} {bytes}"
            );
            assert_eq!(l.alignment(), mode.block_bytes());
            assert!(!l.is_thick());
        }
    }
    for (mode, bytes, block) in [
        (Mode::Standard256, 1, [16, 16, 1]),
        (Mode::Standard256, 2, [16, 8, 1]),
        (Mode::Display256, 8, [8, 4, 1]),
        (Mode::Standard4K, 16, [16, 16, 1]),
        (Mode::Standard64K, 2, [256, 128, 1]),
        (Mode::Display64K, 4, [128, 128, 1]),
        (Mode::Display64K, 16, [64, 64, 1]),
    ] {
        assert_eq!(
            layout(flat(1, 1, 1, bytes, mode)).block(),
            block,
            "{mode:?} {bytes}"
        );
    }
    // Thick blocks of volumes.
    for (mode, bytes, block) in [
        (Mode::Standard4K, 1, [16, 16, 16]),
        (Mode::Standard4K, 4, [8, 16, 8]),
        (Mode::Standard64K, 1, [64, 32, 32]),
        (Mode::Standard64K, 4, [32, 32, 16]),
        (Mode::Standard64KX, 16, [16, 16, 16]),
    ] {
        let l = layout(plane(Shape::ThreeD, [1, 1, 1], 1, bytes, 1, mode));
        assert_eq!(l.block(), block, "{mode:?} {bytes}");
        assert!(l.is_thick());
    }
    // The row-major mode pads rows to 256 bytes.
    let l = layout(flat(30, 20, 1, 4, Mode::Linear));
    assert_eq!((l.block(), l.alignment()), ([64, 1, 1], 256));
    assert_eq!(l.mips()[0].pitch, 64);
}

/// Addresses worked out by hand from the interleaving: in a 64 KiB display
/// block of 4-byte elements the x bits take address bits 2, 3, 6, 9, 11, 13,
/// 15 and the y bits 4, 5, 7, 8, 10, 12, 14. A 4 KiB standard block of
/// 16-byte elements is 16x16 with y in the lower bits.
#[test]
fn hand_checked_addresses() {
    let l = layout(flat(128, 128, 1, 4, Mode::Display64K));
    assert_eq!(l.element_offset(0, 1, 0, 0, 0), Ok(1 << 2));
    assert_eq!(l.element_offset(0, 4, 0, 0, 0), Ok(1 << 6));
    assert_eq!(l.element_offset(0, 0, 4, 0, 0), Ok(1 << 7));
    assert_eq!(l.element_offset(0, 0, 8, 0, 0), Ok(1 << 8));
    assert_eq!(l.element_offset(0, 64, 64, 0, 0), Ok((1 << 15) | (1 << 14)));
    let l = layout(flat(32, 32, 1, 16, Mode::Standard4K));
    assert_eq!(l.element_offset(0, 0, 1, 0, 0), Ok(1 << 4));
    assert_eq!(l.element_offset(0, 1, 0, 0, 0), Ok(1 << 6));
    // Blocks lie row by row: (16, 0) is the second, (0, 16) the third.
    assert_eq!(l.element_offset(0, 16, 0, 0, 0), Ok(4096));
    assert_eq!(l.element_offset(0, 0, 16, 0, 0), Ok(8192));
    // Row-major: 128 elements of 8 bytes fill a row exactly.
    let l = layout(flat(128, 4, 1, 8, Mode::Linear));
    assert_eq!(l.element_offset(0, 3, 1, 0, 0), Ok((128 + 3) * 8));
}

/// A 256x256 BC7 texture with its full chain: in 64 KiB display blocks one
/// tail block holds levels 1 to 8 and level 0 follows; level 1 sits 32 KiB
/// into the tail, 32 elements to the right. 4 KiB blocks hold fewer levels.
#[test]
fn mip_tail_comes_first() {
    let l = layout(plane(
        Shape::TwoD,
        [256, 256, 1],
        9,
        16,
        4,
        Mode::Display64K,
    ));
    assert_eq!((l.first_tail_mip(), l.tail_mips()), (1, 8));
    assert_eq!(
        (l.bytes(), l.slice_bytes(), l.alignment()),
        (131072, 131072, 65536)
    );
    let top = l.mips()[0];
    assert_eq!(
        (top.offset, top.pitch, top.rows, top.tail),
        (65536, 64, 64, None)
    );
    assert_eq!((top.slice_bytes, top.bytes), (65536, 65536));
    assert_eq!(
        (top.width, top.height, top.texel_width, top.texel_height),
        (64, 64, 256, 256)
    );
    let next = l.mips()[1];
    let place = TailPlace {
        offset: 32768,
        x: 32,
        y: 0,
    };
    assert_eq!(
        (next.offset, next.tail, next.in_tail()),
        (32768, Some(place), true)
    );
    assert_eq!(
        (next.pitch, next.rows, next.slice_bytes),
        (32, 64, 32 * 64 * 16)
    );
    assert_eq!((next.width, next.texel_width), (32, 128));
    assert_eq!(l.element_offset(1, 0, 0, 0, 0), Ok(32768));
    // The smallest level is one element of a one-texel level.
    let last = l.mips()[8];
    assert_eq!((last.width, last.height, last.texel_width), (1, 1, 1));
    // Every thin tail level's offset is the address of its first element.
    for mip in 1..9 {
        let level = l.mips()[mip as usize];
        assert_eq!(
            l.element_offset(mip, 0, 0, 0, 0),
            Ok(level.offset),
            "mip {mip}"
        );
    }

    let l = layout(plane(
        Shape::TwoD,
        [256, 256, 1],
        9,
        16,
        4,
        Mode::Standard4K,
    ));
    assert_eq!(l.first_tail_mip(), 3);
    assert_eq!(l.mips()[2].offset, 4096);
    assert_eq!(l.mips()[1].offset, 8192);
    assert_eq!(l.mips()[0].offset, 8192 + 16384);
    // 256-byte blocks never form a tail, and neither does a single level.
    let l = layout(flat(83, 31, 6, 1, Mode::Standard256));
    assert_eq!((l.first_tail_mip(), l.tail_mips()), (6, 0));
    let l = layout(flat(4, 4, 1, 4, Mode::Standard64K));
    assert_eq!((l.first_tail_mip(), l.bytes()), (1, 65536));
}

/// A volume deeper than its block keeps its whole chain in the tail when
/// every level's width and height fit: each slab of depth layers has its own
/// tail block. 16x16x64 in 64 KiB standard blocks of 32x32x16 is four slabs
/// of one block each. The addresses are the address library's.
#[test]
fn deep_volumes_keep_their_chain_in_each_slabs_tail() {
    let l = layout(plane(
        Shape::ThreeD,
        [16, 16, 64],
        4,
        4,
        1,
        Mode::Standard64K,
    ));
    assert_eq!(l.block(), [32, 32, 16]);
    assert!(l.is_thick());
    assert_eq!(
        (l.first_tail_mip(), l.bytes(), l.stored_slices()),
        (0, 4 * 65536, 4)
    );
    for (mip, x, y, z, address) in [
        (0, 0, 0, 0, 32768),
        (0, 0, 0, 15, 42128),
        (0, 0, 0, 16, 65536 + 32768),
        (0, 15, 15, 63, 3 * 65536 + 49148),
        (1, 0, 0, 0, 16384),
        (1, 0, 0, 16, 65536 + 16384),
        (1, 0, 0, 31, 65536 + 25744),
        (2, 0, 0, 0, 4096),
        (3, 1, 1, 7, 3740),
    ] {
        assert_eq!(
            l.element_offset(mip, x, y, z, 0),
            Ok(address),
            "mip {mip} ({x},{y},{z})"
        );
    }
    // A thick tail level is reported by position: its offset is the tail
    // block's, and one layer's share of the block is its slice size.
    let top = l.mips()[0];
    let place = TailPlace {
        offset: 8192,
        x: 16,
        y: 0,
    };
    assert_eq!((top.offset, top.tail), (0, Some(place)));
    assert_eq!(
        (top.slice_bytes, top.depth, top.depth_pitch),
        (4096, 64, 64)
    );
    assert_eq!((l.mips()[1].depth, l.mips()[3].depth), (32, 8));
}

/// Layouts as AMD's address library computes them for this GPU generation,
/// without pipe and bank XOR: the surface size, the first level in the mip
/// tail, and an FNV-1a hash over the address of every element (levels in
/// order, then layers, rows and columns; eight little-endian bytes each).
#[test]
fn layouts_match_the_address_library() {
    use Mode::{Display4K, Display64K, Display256, Linear, Standard4K, Standard64K, Standard256};
    use Shape::{OneD, ThreeD, TwoD};
    // Mode, shape, element bytes, texels per element side, extent, levels,
    // then the expected size, first tail level and hash.
    type Case = (Mode, Shape, u32, u32, [u32; 3], u32, (u64, u32, u64));
    #[rustfmt::skip]
    let cases: [Case; 22] = [
        (Standard256, TwoD, 1, 1, [83, 31, 1], 6, (5120, 6, 0x435e_9cad_96ff_dd81)),
        (Display256, TwoD, 16, 4, [109, 71, 1], 6, (13824, 6, 0xbc2b_b84f_c528_7cb3)),
        (Standard4K, TwoD, 8, 1, [250, 175, 1], 8, (495_616, 4, 0x1e87_7240_4dcd_c56e)),
        (Standard4K, TwoD, 16, 1, [70, 70, 3], 4, (479_232, 4, 0xf948_6d1d_511c_d75c)),
        (Display4K, TwoD, 1, 1, [33, 17, 1], 6, (8192, 1, 0xaae1_0e3f_15a1_892e)),
        (Standard64K, TwoD, 2, 1, [40, 9, 1], 3, (65536, 0, 0xf7a3_7fe6_41c2_d565)),
        (Standard64K, TwoD, 4, 1, [100, 60, 6], 7, (786_432, 1, 0xe6b7_50a0_b65e_09a3)),
        (Standard64K, TwoD, 8, 1, [65, 33, 1], 7, (131_072, 1, 0xdf0a_7ddb_9b93_41ee)),
        (Display64K, TwoD, 16, 4, [256, 256, 1], 9, (131_072, 1, 0xe569_7630_03d8_3e28)),
        (Display64K, TwoD, 4, 1, [100, 60, 1], 7, (131_072, 1, 0xb1db_1641_adaf_77a6)),
        (Display64K, TwoD, 1, 1, [255, 129, 2], 8, (262_144, 1, 0xd706_42f5_9a01_5115)),
        (Linear, TwoD, 4, 1, [30, 20, 1], 5, (10240, 5, 0x819a_9c12_81e6_40f2)),
        (Linear, TwoD, 2, 1, [17, 9, 3], 5, (15360, 5, 0xbf1a_74f1_7803_df62)),
        (Linear, OneD, 4, 1, [1000, 1, 1], 10, (9216, 10, 0xd37e_f9ac_a5de_cab4)),
        (Standard4K, ThreeD, 4, 1, [64, 32, 16], 7, (188_416, 3, 0xf06b_d42a_7183_90e6)),
        (Standard4K, ThreeD, 4, 1, [4, 4, 16], 5, (8192, 0, 0xbf87_001b_ad72_d375)),
        (Standard4K, ThreeD, 1, 1, [1, 1, 17], 2, (8192, 0, 0xcdf4_4ee3_33e5_e03d)),
        (Standard64K, ThreeD, 4, 1, [16, 16, 64], 4, (262_144, 0, 0x5d1a_a7af_c371_e8e5)),
        (Standard64K, ThreeD, 1, 1, [8, 8, 128], 4, (262_144, 0, 0x8fec_f010_6542_6225)),
        (Standard64K, ThreeD, 16, 1, [33, 17, 40], 6, (1_966_080, 3, 0xec9c_fd44_b89c_2fd5)),
        (Linear, ThreeD, 4, 1, [9, 7, 5], 3, (16640, 3, 0x6d5b_351d_4a3d_7b5a)),
        // The same 256x256 BC7 chain in 4 KiB blocks, size only known from
        // the offsets above: three standard levels and one tail block.
        (Standard4K, TwoD, 16, 4, [256, 256, 1], 9, (90112, 3, 0)),
    ];
    for (mode, shape, bytes, texels, extent, mips, expected) in cases {
        let desc = plane(shape, extent, mips, bytes, texels, mode);
        let l = layout(desc);
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for (mip, level) in (0..).zip(l.mips()) {
            let layers = if shape == ThreeD {
                level.depth
            } else {
                extent[2]
            };
            for z in 0..layers {
                for y in 0..level.height {
                    for x in 0..level.width {
                        for byte in l.element_offset(mip, x, y, z, 0).unwrap().to_le_bytes() {
                            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                        }
                    }
                }
            }
        }
        assert_eq!(
            (l.bytes(), l.first_tail_mip()),
            (expected.0, expected.1),
            "{desc:?}"
        );
        if expected.2 != 0 {
            assert_eq!(hash, expected.2, "{desc:?}");
        }
    }
}

/// The XOR variants have the sizes of their plain counterparts, since XOR
/// only moves bytes around inside the same blocks, and refuse addressing.
#[test]
fn xor_modes_have_sizes_but_no_addresses() {
    let twin = |mode: Mode| match mode {
        Mode::Standard4KX => Some(Mode::Standard4K),
        Mode::Display4KX => Some(Mode::Display4K),
        Mode::Standard64KT | Mode::Standard64KX => Some(Mode::Standard64K),
        Mode::Display64KT | Mode::Display64KX => Some(Mode::Display64K),
        _ => None,
    };
    for mode in XOR {
        assert!(mode.has_xor());
        for bytes in [1, 2, 4, 8, 16] {
            for extent in [[1, 1, 1], [33, 17, 1], [300, 200, 6], [1920, 1080, 1]] {
                let mips = full_chain(Shape::TwoD, extent);
                let desc = plane(Shape::TwoD, extent, mips, bytes, 1, mode);
                let l = layout(desc);
                assert!(!l.is_addressable());
                assert_eq!(l.bytes() % l.alignment(), 0);
                assert_eq!(l.bytes(), l.slice_bytes() * u64::from(extent[2]));
                // Standard levels lie after the tail block, each after the
                // next smaller one, and end inside the slice.
                let standard = &l.mips()[..l.first_tail_mip() as usize];
                let mut end = if l.tail_mips() > 0 { l.alignment() } else { 0 };
                for level in standard.iter().rev() {
                    assert_eq!(level.offset, end, "{desc:?}");
                    assert!(level.pitch >= level.width && level.rows >= level.height);
                    end += level.slice_bytes;
                }
                assert_eq!(end, l.slice_bytes(), "{desc:?}");
                if let Some(plain) = twin(mode) {
                    let other = layout(PlaneDesc {
                        mode: plain,
                        ..desc
                    });
                    assert_eq!(l.mips(), other.mips(), "{desc:?}");
                    assert_eq!((l.bytes(), l.block()), (other.bytes(), other.block()));
                }
                assert_eq!(l.element_offset(0, 0, 0, 0, 0), Err(Error::Unsupported));
                assert_eq!(
                    l.element_offset(0, extent[0], 0, 0, 0),
                    Err(Error::OutOfRange)
                );
                let pitch =
                    LinearPitch::packed(extent[0] as usize * bytes as usize, extent[1] as usize);
                let mut tiled = vec![0; l.bytes() as usize];
                let mut rows = vec![0; pitch.depth];
                assert_eq!(
                    l.detile(&tiled, 0, 0, &mut rows, pitch),
                    Err(Error::Unsupported)
                );
                assert_eq!(
                    l.tile(&mut tiled, 0, 0, &rows, pitch),
                    Err(Error::Unsupported)
                );
            }
        }
    }
    assert!(ADDRESSABLE.iter().all(|mode| !mode.has_xor()));
}

/// Depth planes of small elements enter the mip tail as if their elements
/// were four bytes: a 64x64 level of one-byte depth would fit half a block
/// (128x256) but has to wait until it is 64x128 or smaller.
#[test]
fn small_depth_elements_enter_the_tail_late() {
    let depth = layout(flat(128, 128, 8, 1, Mode::Depth64KX));
    let colour = layout(flat(128, 128, 8, 1, Mode::Standard64KX));
    assert_eq!(colour.first_tail_mip(), 0);
    assert_eq!(depth.first_tail_mip(), 1);
    // Four-byte depth is not affected.
    let depth = layout(flat(64, 128, 8, 4, Mode::Depth64KX));
    assert_eq!(depth.first_tail_mip(), 0);
}

#[test]
fn multisampled_textures_have_sizes_but_no_addresses() {
    let single = layout(flat(256, 256, 1, 4, Mode::Linear));
    assert_eq!(single.bytes(), 256 * 256 * 4);
    for samples in [2, 4, 8] {
        let desc = PlaneDesc {
            samples,
            ..flat(256, 256, 1, 4, Mode::Linear)
        };
        // Row-major textures cannot be multisampled.
        assert_eq!(PlaneLayout::new(&desc), Err(Error::Invalid));
        for mode in [Mode::Render64KX, Mode::Standard4K, Mode::Depth64KX] {
            let l = layout(PlaneDesc { mode, ..desc });
            assert_eq!(l.bytes(), single.bytes() * u64::from(samples), "{mode:?}");
            let [w, h, _] = l.block();
            assert_eq!(u64::from(w * h * 4 * samples), mode.block_bytes());
            assert!(!l.is_addressable());
            assert_eq!(
                l.element_offset(0, 0, 0, 0, samples - 1),
                Err(Error::Unsupported)
            );
            assert_eq!(
                l.element_offset(0, 0, 0, 0, samples),
                Err(Error::OutOfRange)
            );
        }
        // Not with mips, and not for volumes.
        let chain = PlaneDesc {
            mips: 2,
            mode: Mode::Render64KX,
            ..desc
        };
        assert_eq!(PlaneLayout::new(&chain), Err(Error::Invalid));
        let volume = PlaneDesc {
            samples,
            ..plane(Shape::ThreeD, [8, 8, 8], 1, 4, 1, Mode::Standard64K)
        };
        assert_eq!(PlaneLayout::new(&volume), Err(Error::Invalid));
    }
    // Two samples give the extra factor to the height: 64x128 per 64 KiB.
    let two = PlaneDesc {
        samples: 2,
        ..flat(8, 8, 1, 4, Mode::Render64KX)
    };
    assert_eq!(layout(two).block(), [64, 128, 1]);
}

/// The smaller levels lie first, so they can be read from memory that holds
/// only them.
#[test]
fn smaller_mips_need_only_the_start_of_the_plane() {
    for mode in [Mode::Standard4K, Mode::Standard64K] {
        let l = layout(plane(Shape::TwoD, [1024, 1024, 1], 9, 8, 4, mode));
        assert_eq!(l.bytes_from_mip(0), l.bytes());
        assert_eq!(l.bytes_from_mip(9), l.bytes());
        let needs: Vec<u64> = (0..9).map(|mip| l.bytes_from_mip(mip)).collect();
        assert!(needs.windows(2).all(|pair| pair[0] >= pair[1]), "{needs:?}");
        assert!(needs[3] * 4 < l.bytes(), "{needs:?}");
        let tiled = noise(l.bytes() as usize, 3);
        for mip in 1..9 {
            let level = l.mips()[mip as usize];
            let pitch = LinearPitch::packed(level.width as usize * 8, level.height as usize);
            let read = |tiled: &[u8]| {
                let mut rows = vec![0; pitch.depth];
                l.detile(tiled, mip, 0, &mut rows, pitch).map(|()| rows)
            };
            let len = l.bytes_from_mip(mip) as usize;
            assert_eq!(read(&tiled[..len]), read(&tiled));
            assert_eq!(read(&tiled[..len - 1]), Err(Error::TooSmall));
            // Every element of the level lies inside that prefix.
            for (x, y) in [(0, 0), (level.width - 1, level.height - 1)] {
                assert!(l.element_offset(mip, x, y, 0, 0).unwrap() + 8 <= len as u64);
            }
        }
    }
    // Slices each hold their own chain, so an array needs everything.
    let l = layout(plane(
        Shape::TwoD,
        [1024, 1024, 2],
        9,
        8,
        4,
        Mode::Standard4K,
    ));
    assert_eq!(l.bytes_from_mip(4), l.bytes());
}

#[test]
fn bad_descriptions_are_refused() {
    let good = flat(4, 4, 1, 4, Mode::Standard4K);
    assert!(PlaneLayout::new(&good).is_ok());
    let bad = [
        PlaneDesc { width: 0, ..good },
        PlaneDesc { height: 0, ..good },
        PlaneDesc {
            depth_or_slices: 0,
            ..good
        },
        PlaneDesc { mips: 0, ..good },
        PlaneDesc {
            mips: MAX_MIPS + 1,
            ..good
        },
        PlaneDesc {
            element_bytes: 12,
            ..good
        },
        PlaneDesc {
            element_bytes: 32,
            ..good
        },
        PlaneDesc {
            element_texels: 2,
            ..good
        },
        PlaneDesc { samples: 3, ..good },
        PlaneDesc {
            samples: 16,
            ..good
        },
        PlaneDesc {
            width: MAX_EXTENT + 1,
            ..good
        },
        PlaneDesc {
            height: u32::MAX,
            ..good
        },
        PlaneDesc {
            depth_or_slices: u32::MAX,
            ..good
        },
        // A row has no height.
        PlaneDesc {
            shape: Shape::OneD,
            ..good
        },
        // Volumes have no 256-byte, display or 4 KiB render layouts.
        plane(Shape::ThreeD, [4, 4, 4], 1, 4, 1, Mode::Standard256),
        plane(Shape::ThreeD, [4, 4, 4], 1, 4, 1, Mode::Display4K),
        plane(Shape::ThreeD, [4, 4, 4], 1, 4, 1, Mode::Display64K),
        plane(Shape::ThreeD, [4, 4, 4], 1, 4, 1, Mode::Render4KX),
    ];
    for desc in bad {
        assert_eq!(PlaneLayout::new(&desc), Err(Error::Invalid), "{desc:?}");
    }
    // The largest texture the GPU can have is laid out without overflow.
    for mode in [Mode::Linear, Mode::Display64KX] {
        let l = layout(flat(MAX_EXTENT, MAX_EXTENT, 15, 16, mode));
        assert!(l.bytes() >= u64::from(MAX_EXTENT) * u64::from(MAX_EXTENT) * 16);
    }
}

#[test]
fn positions_outside_the_texture_are_refused() {
    let l = layout(flat(64, 32, 2, 4, Mode::Standard4K));
    assert_eq!(l.element_offset(0, 64, 0, 0, 0), Err(Error::OutOfRange));
    assert_eq!(l.element_offset(0, 0, 32, 0, 0), Err(Error::OutOfRange));
    assert_eq!(l.element_offset(0, 0, 0, 1, 0), Err(Error::OutOfRange));
    assert_eq!(l.element_offset(0, 0, 0, 0, 1), Err(Error::OutOfRange));
    assert_eq!(l.element_offset(2, 0, 0, 0, 0), Err(Error::OutOfRange));
    assert_eq!(l.element_offset(1, 32, 0, 0, 0), Err(Error::OutOfRange));
    assert!(l.element_offset(1, 31, 15, 0, 0).is_ok());
    assert_eq!((l.mip(2), l.subresource_depth(2)), (None, 0));
    // A volume's depth halves with its levels.
    let l = layout(plane(Shape::ThreeD, [8, 8, 8], 2, 4, 1, Mode::Standard4K));
    assert!(l.element_offset(0, 0, 0, 7, 0).is_ok());
    assert_eq!(l.element_offset(1, 0, 0, 4, 0), Err(Error::OutOfRange));
    assert_eq!((l.subresource_slices(), l.subresource_depth(1)), (1, 4));
}

#[test]
fn copies_refuse_buffers_and_pitches_that_are_too_small() {
    let l = layout(flat(64, 64, 1, 4, Mode::Standard4K));
    let exact = LinearPitch::packed(256, 64);
    let mut rows = vec![0; 64 * 256];
    let mut tiled = vec![0; l.bytes() as usize];
    assert_eq!(
        l.detile(&tiled[..100], 0, 0, &mut rows, exact),
        Err(Error::TooSmall)
    );
    assert_eq!(
        l.tile(&mut tiled[..100], 0, 0, &rows, exact),
        Err(Error::TooSmall)
    );
    let short = rows.len() - 1;
    assert_eq!(
        l.detile(&tiled, 0, 0, &mut rows[..short], exact),
        Err(Error::TooSmall)
    );
    assert_eq!(
        l.tile(&mut tiled, 0, 0, &rows[..short], exact),
        Err(Error::TooSmall)
    );
    let narrow = LinearPitch { row: 255, depth: 0 };
    assert_eq!(
        l.detile(&tiled, 0, 0, &mut rows, narrow),
        Err(Error::TooSmall)
    );
    let huge = LinearPitch {
        row: usize::MAX,
        depth: 0,
    };
    assert_eq!(
        l.detile(&tiled, 0, 0, &mut rows, huge),
        Err(Error::TooSmall)
    );
    assert_eq!(l.tile(&mut tiled, 0, 0, &rows, huge), Err(Error::TooSmall));
    assert_eq!(
        l.detile(&tiled, 1, 0, &mut rows, exact),
        Err(Error::OutOfRange)
    );
    assert_eq!(
        l.detile(&tiled, 0, 1, &mut rows, exact),
        Err(Error::OutOfRange)
    );
    // The depth pitch of a flat texture is never read.
    let ignored = LinearPitch { row: 256, depth: 0 };
    assert_eq!(l.detile(&tiled, 0, 0, &mut rows, ignored), Ok(()));
}

/// Depth layers of a volume must not overlap in the plain buffer: a 4x2
/// level of 4-byte elements with 20-byte rows needs 36 bytes per layer.
#[test]
fn volume_copies_check_the_depth_pitch() {
    for mode in [Mode::Linear, Mode::Standard4K] {
        let l = layout(plane(Shape::ThreeD, [4, 2, 2], 1, 4, 1, mode));
        for depth in [0, 16, 35, usize::MAX] {
            let pitch = LinearPitch { row: 20, depth };
            let mut tiled = vec![0xcc; l.bytes() as usize];
            let mut rows = [0x42; 72];
            assert_eq!(
                l.detile(&tiled, 0, 0, &mut rows, pitch),
                Err(Error::TooSmall)
            );
            assert_eq!(l.tile(&mut tiled, 0, 0, &rows, pitch), Err(Error::TooSmall));
            assert!(rows.iter().all(|&byte| byte == 0x42));
            assert!(tiled.iter().all(|&byte| byte == 0xcc));
        }
        for depth in [36, 40, 64] {
            let pitch = LinearPitch { row: 20, depth };
            assert_eq!(l.linear_bytes(0, pitch), Ok(depth + 36));
            let rows = noise(depth + 36, depth as u64);
            let mut tiled = vec![0; l.bytes() as usize];
            let mut back = vec![0xcc; rows.len()];
            l.tile(&mut tiled, 0, 0, &rows, pitch).unwrap();
            l.detile(&tiled, 0, 0, &mut back, pitch).unwrap();
            for start in [0, 20, depth, depth + 20] {
                assert_eq!(back[start..start + 16], rows[start..start + 16]);
            }
        }
        // A volume is one subresource per level: there is no slice 1.
        let pitch = LinearPitch { row: 16, depth: 32 };
        let tiled = vec![0; l.bytes() as usize];
        assert_eq!(
            l.detile(&tiled, 0, 1, &mut [0; 64], pitch),
            Err(Error::OutOfRange)
        );
    }
}

#[test]
fn modes_keep_their_hardware_numbers() {
    let numbers = [0, 1, 2, 5, 6, 9, 10, 17, 18, 21, 22, 23, 24, 25, 26, 27];
    for (mode, number) in Mode::ALL.into_iter().zip(numbers) {
        assert_eq!(mode.number(), number);
        assert_eq!(Mode::from_number(number), Some(mode));
    }
    for unused in [3, 4, 7, 8, 11, 16, 19, 20, 28, 255] {
        assert_eq!(Mode::from_number(unused), None);
    }
    assert_eq!(Mode::Linear.block_bytes(), 1);
    assert_eq!(Mode::Display256.block_bytes(), 256);
    assert_eq!(Mode::Render4KX.block_bytes(), 4096);
    assert_eq!(Mode::Depth64KX.block_bytes(), 65536);
    assert_eq!(Mode::Depth64KX.family(), Family::Depth);
    assert_eq!(Mode::Render64KX.family(), Family::Render);
    // The mode a tail level is reported in.
    assert_eq!(Mode::Standard4K.tail_number(), 1);
    assert_eq!(Mode::Standard64KX.tail_number(), 1);
    assert_eq!(Mode::Display64K.tail_number(), 2);
    assert_eq!(Mode::Display64KT.tail_number(), 2);
    assert_eq!(Mode::Render64KX.tail_number(), 3);
    assert_eq!(Mode::Depth64KX.tail_number(), 24);
}

#[test]
fn suggestions_follow_the_use() {
    for shape in [Shape::TwoD, Shape::ThreeD] {
        assert_eq!(suggested_mode(shape, Usage::Sampled), Mode::Standard4K);
        assert_eq!(suggested_mode(shape, Usage::RenderTarget), Mode::Render64KX);
        assert_eq!(suggested_mode(shape, Usage::DepthStencil), Mode::Depth64KX);
    }
    // A row has no tiled layout, whatever its use.
    for usage in [Usage::Sampled, Usage::RenderTarget, Usage::DepthStencil] {
        assert_eq!(suggested_mode(Shape::OneD, usage), Mode::Linear);
    }
    assert_eq!(suggested_mode(Shape::TwoD, Usage::Sampled).number(), 5);
    assert_eq!(
        suggested_mode(Shape::TwoD, Usage::RenderTarget).number(),
        27
    );
    assert_eq!(
        suggested_mode(Shape::TwoD, Usage::DepthStencil).number(),
        24
    );
}

#[test]
fn tile_maps_list_standard_levels_and_the_packed_tail() {
    // 256x256 BC7 with its full chain: one tile for the tail, one for the
    // largest level.
    let l = layout(plane(
        Shape::TwoD,
        [256, 256, 1],
        9,
        16,
        4,
        Mode::Display64K,
    ));
    let map = l.tile_map().unwrap();
    assert_eq!(
        (map.total_tiles, map.standard_mips, map.packed_mips),
        (2, 1, 8)
    );
    assert_eq!((map.packed_tiles, map.packed_start), (1, 0));
    assert_eq!(map.tile_texels, [256, 256, 1]);
    let one = TileRegion {
        width: 1,
        height: 1,
        depth: 1,
        start: 1,
    };
    assert_eq!(map.regions, [one]);

    // One level and no tail: 1024x1024 texels of four bytes are 8x8 tiles.
    let l = layout(flat(1024, 1024, 1, 4, Mode::Standard64K));
    let map = l.tile_map().unwrap();
    assert_eq!(
        (
            map.total_tiles,
            map.standard_mips,
            map.packed_mips,
            map.packed_tiles
        ),
        (64, 1, 0, 0)
    );
    assert_eq!(map.tile_texels, [128, 128, 1]);
    let whole = |start| TileRegion {
        width: 8,
        height: 8,
        depth: 1,
        start,
    };
    assert_eq!(map.regions, [whole(0)]);
    // Without a tail every slice of an array is listed.
    let l = layout(plane(
        Shape::TwoD,
        [1024, 1024, 3],
        1,
        4,
        1,
        Mode::Standard64KX,
    ));
    let map = l.tile_map().unwrap();
    assert_eq!(map.total_tiles, 192);
    assert_eq!(map.regions, [whole(0), whole(64), whole(128)]);

    // A chain of 512x256 four-byte texels: levels 0 to 2 are standard (4x2
    // tiles, 2x1 and 1x1; level 2 is as wide as a block, too wide for the
    // tail). The tail is tile 0, then level 2, level 1 and level 0.
    let l = layout(plane(
        Shape::TwoD,
        [512, 256, 5],
        10,
        4,
        1,
        Mode::Standard64K,
    ));
    let map = l.tile_map().unwrap();
    assert_eq!(
        (map.standard_mips, map.packed_mips, map.packed_tiles),
        (3, 7, 1)
    );
    assert_eq!(map.total_tiles, 5 * 12);
    let region = |width, height, start| TileRegion {
        width,
        height,
        depth: 1,
        start,
    };
    let first = [region(4, 2, 4), region(2, 1, 2), region(1, 1, 1)];
    // With a tail, two slices are listed and the second is zeroes.
    assert_eq!(map.regions.len(), 6);
    assert_eq!(map.regions[..3], first);
    assert_eq!(map.regions[3..], [TileRegion::default(); 3]);

    // Every level in the tail: the whole slice is packed.
    let l = layout(flat(40, 9, 3, 2, Mode::Standard64K));
    let map = l.tile_map().unwrap();
    assert_eq!(
        (
            map.total_tiles,
            map.standard_mips,
            map.packed_mips,
            map.packed_tiles
        ),
        (1, 0, 3, 1)
    );
    assert!(map.regions.is_empty());

    // Only the 64 KiB modes have 64 KiB tiles; thick volumes are not built.
    for mode in [
        Mode::Linear,
        Mode::Standard256,
        Mode::Standard4K,
        Mode::Render4KX,
    ] {
        assert_eq!(
            layout(flat(256, 256, 1, 4, mode)).tile_map(),
            Err(Error::NotTiled)
        );
    }
    let l = layout(plane(
        Shape::ThreeD,
        [64, 64, 64],
        1,
        4,
        1,
        Mode::Standard64K,
    ));
    assert_eq!(l.tile_map(), Err(Error::Unsupported));
}

#[test]
fn formats_give_element_sizes() {
    let info =
        |format| format_info(format).map(|f| (f.element_bytes, f.element_texels, f.stencil_plane));
    assert_eq!(info(2), Some((16, 1, false))); // four 32-bit floats
    assert_eq!(info(10), Some((8, 1, false))); // four 16-bit floats
    assert_eq!(info(20), Some((4, 1, true))); // 32-bit depth and stencil
    assert_eq!(info(28), Some((4, 1, false))); // RGBA8
    assert_eq!(info(40), Some((4, 1, false))); // 32-bit depth alone
    assert_eq!(info(45), Some((4, 1, true))); // 24-bit depth and stencil
    assert_eq!(info(55), Some((2, 1, false))); // 16-bit depth alone
    assert_eq!(info(61), Some((1, 1, false))); // R8
    assert_eq!(info(71), Some((8, 4, false))); // BC1
    assert_eq!(info(77), Some((16, 4, false))); // BC3
    assert_eq!(info(80), Some((8, 4, false))); // BC4
    assert_eq!(info(83), Some((16, 4, false))); // BC5
    assert_eq!(info(87), Some((4, 1, false))); // BGRA8
    assert_eq!(info(98), Some((16, 4, false))); // BC7
    assert_eq!(info(115), Some((2, 1, false))); // BGRA4
    assert_eq!(info(116), Some((4, 1, false))); // console 10-bit float
    assert_eq!(info(118), Some((2, 1, true))); // console 16-bit depth and stencil
    for none in [0, 6, 66, 68, 69, 100, 107, 121, 190, u32::MAX] {
        assert_eq!(format_info(none), None, "{none}");
    }
}

fn texture(width: u32, height: u32, mips: u32, format: u32, mode: Mode) -> TextureDesc {
    TextureDesc {
        shape: Shape::TwoD,
        width,
        height,
        depth_or_slices: 1,
        mips,
        format,
        samples: 1,
        mode,
    }
}

/// A 1920x1080 depth buffer with stencil: four-byte depth in 128x128 blocks,
/// then one-byte stencil in 256x256 blocks.
#[test]
fn depth_stencil_is_two_planes() {
    let t = Texture::new(&texture(1920, 1080, 1, 20, Mode::Depth64KX)).unwrap();
    let [depth, stencil] = t.planes() else {
        panic!("two planes expected");
    };
    assert_eq!((depth.role, depth.offset), (PlaneRole::Main, 0));
    assert_eq!(depth.layout.bytes(), 1920 * 1152 * 4);
    assert_eq!(
        (stencil.role, stencil.offset),
        (PlaneRole::Stencil, 1920 * 1152 * 4)
    );
    assert_eq!(
        (stencil.layout.bytes(), stencil.layout.element_bytes()),
        (2048 * 1280, 1)
    );
    assert_eq!(t.bytes(), 1920 * 1152 * 4 + 2048 * 1280);
    assert_eq!((t.alignment(), t.mips()), (65536, 1));
    assert!(!depth.layout.is_addressable());
    assert_eq!(t.element_offset(1, 0, 0, 0, 0, 0), Err(Error::Unsupported));
    assert_eq!(t.plane(2).err(), Some(Error::OutOfRange));
}

#[test]
fn textures_count_out_a_full_chain_and_number_subresources() {
    let t = Texture::new(&texture(256, 256, 0, 98, Mode::Display64K)).unwrap();
    assert_eq!(
        (t.bytes(), t.alignment(), t.mips(), t.planes().len()),
        (131072, 65536, 9, 1)
    );
    let l = &t.planes()[0].layout;
    assert_eq!(
        (l.first_tail_mip(), l.tail_mips(), l.element_bytes()),
        (1, 8, 16)
    );
    assert_eq!(
        (t.subresource(0), t.subresource(8), t.subresource(9)),
        (Some((0, 0)), Some((8, 0)), None)
    );
    assert_eq!(t.bytes_for_copy(0, 0), Ok(131072));
    assert_eq!(t.bytes_for_copy(0, 1), Ok(65536));

    let cube = TextureDesc {
        depth_or_slices: 6,
        ..texture(100, 60, 0, 28, Mode::Standard64K)
    };
    let t = Texture::new(&cube).unwrap();
    assert_eq!((t.mips(), t.bytes()), (7, 786_432));
    assert_eq!(
        (t.subresource(7), t.subresource(41), t.subresource(42)),
        (Some((0, 1)), Some((6, 5)), None)
    );

    // A volume's chain follows its largest side, depth included.
    let volume = TextureDesc {
        shape: Shape::ThreeD,
        depth_or_slices: 64,
        ..texture(16, 16, 0, 28, Mode::Standard64K)
    };
    let t = Texture::new(&volume).unwrap();
    assert_eq!(
        (t.mips(), t.subresource(6), t.subresource(7)),
        (7, Some((6, 0)), None)
    );
    // An array's slice count does not lengthen the chain.
    let array = TextureDesc {
        depth_or_slices: 64,
        ..texture(16, 16, 0, 28, Mode::Standard64K)
    };
    assert_eq!(Texture::new(&array).unwrap().mips(), 5);

    // Zero samples count as one; unknown formats are refused.
    let unsampled = TextureDesc {
        samples: 0,
        ..texture(8, 8, 1, 28, Mode::Standard4K)
    };
    assert!(Texture::new(&unsampled).is_ok());
    assert_eq!(
        Texture::new(&texture(8, 8, 1, 0, Mode::Standard4K)),
        Err(Error::Invalid)
    );
}

/// Copies and element offsets through the whole texture reach the right
/// plane: stencil lies after depth.
#[test]
fn texture_copies_and_offsets_add_the_plane_offset() {
    let t = Texture::new(&texture(100, 60, 3, 45, Mode::Standard4K)).unwrap();
    let stencil = &t.planes()[1];
    assert_eq!(stencil.offset % 4096, 0);
    assert_eq!(stencil.offset, t.planes()[0].layout.bytes());
    let mut tiled = vec![0xee; t.bytes() as usize];
    for (plane, bytes) in [(0, 4), (1, 1)] {
        for mip in 0..3 {
            let level = t.planes()[plane as usize].layout.mips()[mip as usize];
            let pitch = LinearPitch::packed(level.width as usize * bytes, level.height as usize);
            let rows = noise(pitch.depth, u64::from(plane * 8 + mip) + 1);
            t.tile(&mut tiled, plane, mip, 0, &rows, pitch).unwrap();
            let mut back = vec![0; rows.len()];
            t.detile(&tiled, plane, mip, 0, &mut back, pitch).unwrap();
            assert_eq!(back, rows);
            let at = t.element_offset(plane, mip, 1, 0, 0, 0).unwrap() as usize;
            assert_eq!(tiled[at..at + bytes], rows[bytes..2 * bytes]);
            assert_eq!(at >= stencil.offset as usize, plane == 1);
        }
    }
    // Depth was not disturbed by the stencil copies.
    let level = t.planes()[0].layout.mips()[0];
    let pitch = LinearPitch::packed(level.width as usize * 4, level.height as usize);
    let mut back = vec![0; pitch.depth];
    t.detile(&tiled, 0, 0, 0, &mut back, pitch).unwrap();
    assert_eq!(back, noise(pitch.depth, 1));
    // A buffer that ends before the plane is too small, not a panic.
    assert_eq!(
        t.detile(&tiled[..100], 1, 0, 0, &mut back, pitch),
        Err(Error::TooSmall)
    );
    assert_eq!(
        t.tile(&mut tiled, 2, 0, 0, &back, pitch),
        Err(Error::OutOfRange)
    );
}
