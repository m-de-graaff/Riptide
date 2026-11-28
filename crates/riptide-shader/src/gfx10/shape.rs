//! What each operation's operands are like, worked out from its mnemonic.
//!
//! The encoding of an instruction says which register a field names but not
//! how many registers the operand spans, how many sources the operation
//! takes, or whether a field is used at all. Those are properties of the
//! operation. The instruction set names its operations systematically (the
//! last part of a mnemonic is the type of its operands, `x2` doubles a
//! memory access, and so on), so the properties are derived here from the
//! names, with the exceptions listed one by one, instead of being stored for
//! each of the twelve hundred opcodes.

/// How many 32-bit registers a value of the named type occupies.
fn type_words(token: &str) -> u8 {
    match token {
        "b64" | "u64" | "i64" | "f64" => 2,
        "b96" => 3,
        "b128" => 4,
        _ => 1,
    }
}

/// The last part of a mnemonic, which usually names its operand type.
fn last_token(name: &str) -> &str {
    name.rsplit('_').next().unwrap_or("")
}

/// The widths in registers of a scalar operation's result, first source and
/// second source.
pub(super) fn scalar_widths(name: &str) -> (u8, u8, u8) {
    match name {
        // Shifts and bit-field extracts of a 64-bit value take a 32-bit count.
        "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" | "s_bfe_u64" | "s_bfe_i64" => (2, 2, 1),
        // Bit tests of a 64-bit value take a 32-bit bit number.
        "s_bitcmp0_b64" | "s_bitcmp1_b64" => (2, 2, 1),
        // These build a 64-bit result from 32-bit sources.
        "s_bfm_b64" | "s_bitset0_b64" | "s_bitset1_b64" | "s_bitreplicate_b64_b32" => (2, 1, 1),
        // Counts and searches over a 64-bit value give a 32-bit result.
        "s_bcnt0_i32_b64" | "s_bcnt1_i32_b64" | "s_ff0_i32_b64" | "s_ff1_i32_b64"
        | "s_flbit_i32_b64" | "s_flbit_i32_i64" => (1, 2, 2),
        _ => {
            let width = type_words(last_token(name));
            (width, width, width)
        }
    }
}

/// The widths in registers of a vector operation's result and of its three
/// sources. Lane masks are not covered here: their width depends on the wave
/// size, and the decoder places them.
pub(super) fn vector_widths(name: &str) -> (u8, [u8; 3]) {
    let tokens: Vec<&str> = name.split('_').collect();
    match name {
        // 64-bit shifts take the 32-bit count first.
        "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => (2, [1, 2, 2]),
        // These take a 32-bit second source next to a 64-bit first.
        "v_ldexp_f64" | "v_trig_preop_f64" | "v_cmp_class_f64" | "v_cmpx_class_f64" => {
            (2, [2, 1, 2])
        }
        "v_mad_u64_u32" | "v_mad_i64_i32" => (2, [1, 1, 2]),
        "v_qsad_pk_u16_u8" | "v_mqsad_pk_u16_u8" => (2, [2, 1, 2]),
        "v_mqsad_u32_u8" => (4, [2, 1, 4]),
        "v_frexp_exp_i32_f64" => (1, [2, 2, 2]),
        // A plain conversion is named result type, then source type.
        _ if tokens.len() == 4 && tokens[1] == "cvt" => {
            (type_words(tokens[2]), [type_words(tokens[3]); 3])
        }
        _ => {
            let width = type_words(last_token(name));
            (width, [width; 3])
        }
    }
}

/// How many 32-bit registers the data of a memory instruction occupies: what
/// a load returns, a store writes, or an atomic operation takes.
pub(super) fn data_words(name: &str) -> u8 {
    let mut words = 1u8;
    for token in name.split('_') {
        words = match token {
            "dwordx2" | "x2" | "xy" => 2,
            "dwordx3" | "xyz" => 3,
            "dwordx4" | "xyzw" => 4,
            "dwordx8" => 8,
            "dwordx16" => 16,
            "b64" | "u64" | "i64" | "f64" | "b96" | "b128" => type_words(token),
            _ => words,
        };
    }
    // A compare-and-swap carries the new value and the value to compare with.
    if name.contains("cmpswap") {
        words *= 2;
    }
    // Formatted loads of 16-bit data pack two components into a register.
    if name.contains("format_d16") {
        words = words.div_ceil(2);
    }
    words
}

/// Whether a vector operation takes no operands at all. The console compiler
/// is reported to store a hash of the shader in the unused fields of such
/// instructions, so their fields must not be read as registers.
pub(super) fn vector_no_operands(name: &str) -> bool {
    matches!(name, "v_nop" | "v_pipeflush" | "v_clrexcp")
}

/// Whether an operation of the three-source format proper takes only two
/// sources.
pub(super) fn vop3_two_sources(name: &str) -> bool {
    const TWO: &[&str] = &[
        // 64-bit arithmetic, which has no shorter encoding.
        "v_add_f64",
        "v_mul_f64",
        "v_min_f64",
        "v_max_f64",
        "v_ldexp_f64",
        "v_trig_preop_f64",
        "v_lshlrev_b64",
        "v_lshrrev_b64",
        "v_ashrrev_i64",
        // 32-bit multiplies, shifts and additions without carry.
        "v_mul_lo_u32",
        "v_mul_hi_u32",
        "v_mul_lo_i32",
        "v_mul_hi_i32",
        "v_ldexp_f32",
        "v_add_nc_i32",
        "v_sub_nc_i32",
        "v_add_co_u32",
        "v_sub_co_u32",
        "v_subrev_co_u32",
        // 16-bit arithmetic.
        "v_add_nc_u16",
        "v_sub_nc_u16",
        "v_add_nc_i16",
        "v_sub_nc_i16",
        "v_mul_lo_u16",
        "v_lshlrev_b16",
        "v_lshrrev_b16",
        "v_ashrrev_i16",
        "v_max_u16",
        "v_max_i16",
        "v_min_u16",
        "v_min_i16",
        // Packing and conversion.
        "v_pack_b32_f16",
        "v_cvt_pknorm_i16_f16",
        "v_cvt_pknorm_u16_f16",
        "v_cvt_pknorm_i16_f32",
        "v_cvt_pknorm_u16_f32",
        "v_cvt_pk_u16_u32",
        "v_cvt_pk_i16_i32",
        // Bit and lane operations.
        "v_bfm_b32",
        "v_bcnt_u32_b32",
        "v_mbcnt_lo_u32_b32",
        "v_mbcnt_hi_u32_b32",
        "v_readlane_b32",
        "v_writelane_b32",
        "v_interp_p1ll_f16",
    ];
    TWO.contains(&name)
}

/// Whether an operation in the three-source encoding has a second, scalar
/// result where others have modifiers (the form AMD calls VOP3B).
pub(super) fn has_second_result(name: &str) -> bool {
    matches!(
        name,
        "v_add_co_u32"
            | "v_sub_co_u32"
            | "v_subrev_co_u32"
            | "v_add_co_ci_u32"
            | "v_sub_co_ci_u32"
            | "v_subrev_co_ci_u32"
            | "v_div_scale_f32"
            | "v_div_scale_f64"
            | "v_mad_u64_u32"
            | "v_mad_i64_i32"
    )
}

/// Whether an operation reads a lane mask as its last source: the select of
/// a conditional move, or a carry in.
pub(super) fn takes_mask(name: &str) -> bool {
    name == "v_cndmask_b32" || name.ends_with("_co_ci_u32")
}

/// Whether a two-source vector operation carries a 32-bit constant in a
/// second word, and if so whether the constant is the middle operand (`mk`)
/// or the last (`ak`).
pub(super) fn constant_position(name: &str) -> Option<ConstantPosition> {
    let tail = name.strip_prefix("v_mad").or(name.strip_prefix("v_fma"))?;
    if tail.starts_with("mk_") {
        Some(ConstantPosition::Middle)
    } else if tail.starts_with("ak_") {
        Some(ConstantPosition::Last)
    } else {
        None
    }
}

/// Where the constant of a multiply-add with constant goes among the sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConstantPosition {
    /// The constant is the multiplier: source, constant, source.
    Middle,
    /// The constant is the addend: source, source, constant.
    Last,
}

/// Whether a group-shared memory operation works on two addresses, with one
/// offset byte for each.
pub(super) fn lds_pair(name: &str) -> bool {
    name.split('_').any(|token| {
        token.starts_with("read2") || token.starts_with("write2") || token.starts_with("wrxchg2")
    })
}

/// The operands of a group-shared memory operation.
pub(super) struct LdsShape {
    /// Whether the operation returns a value.
    pub returns: bool,
    /// Whether the operation takes an address register.
    pub address: bool,
    /// How many data registers the operation takes (0 to 2).
    pub data: u8,
    /// The width of one value in registers.
    pub width: u8,
}

/// Works out the operands of a group-shared memory operation.
pub(super) fn lds_shape(name: &str) -> LdsShape {
    let global_wave_sync = name.starts_with("ds_gws_");
    let no_operand_sync = matches!(
        name,
        "ds_gws_sema_v" | "ds_gws_sema_p" | "ds_gws_sema_release_all"
    );
    let returns = name.contains("read")
        || name.contains("_rtn")
        || name.contains("permute")
        || matches!(
            name,
            "ds_swizzle_b32" | "ds_consume" | "ds_append" | "ds_ordered_count"
        );
    let address = !(global_wave_sync
        || name.contains("addtid")
        || matches!(name, "ds_nop" | "ds_consume" | "ds_append"));
    let data = if name.contains("read")
        || name.contains("src2")
        || no_operand_sync
        || matches!(
            name,
            "ds_swizzle_b32" | "ds_nop" | "ds_consume" | "ds_append" | "ds_ordered_count"
        ) {
        0
    } else if lds_pair(name)
        || name.contains("cmpst")
        || name.contains("mskor")
        || name.contains("wrap")
    {
        2
    } else {
        1
    };
    LdsShape {
        returns,
        address,
        data,
        width: type_words(last_token(name)),
    }
}

/// How many address registers an image operation reads, for a dimension and
/// coordinate size.
///
/// The address is, in order: a texel offset, a LOD bias and a depth
/// comparison value where the operation has them (one register each), the
/// gradients, the coordinates, and then a LOD, a LOD clamp or a mip level.
/// With 16-bit coordinates (`a16`) the gradients and the coordinates are
/// packed two to a register.
pub(super) fn image_address_words(name: &str, dimension: u8, a16: bool) -> u8 {
    // Coordinates and gradient components per direction, by dimension: 1D,
    // 2D, 3D, cube, 1D array, 2D array, 2D multisampled, 2D multisampled
    // array. A multisampled image's last coordinate is the sample number.
    const SHAPES: [(u8, u8); 8] = [
        (1, 1),
        (2, 2),
        (3, 3),
        (3, 2),
        (2, 1),
        (3, 2),
        (3, 2),
        (4, 2),
    ];
    match name {
        "image_bvh_intersect_ray" => return if a16 { 8 } else { 11 },
        "image_bvh64_intersect_ray" => return if a16 { 9 } else { 12 },
        "image_get_resinfo" => return 1,
        _ => {}
    }
    let (coordinates, gradient) = SHAPES[usize::from(dimension & 7)];
    let mut whole = 0u8;
    let mut gradients = 0u8;
    let mut packed = coordinates;
    let sampling = name
        .strip_prefix("image_sample")
        .or_else(|| name.strip_prefix("image_gather4"));
    if let Some(variant) = sampling {
        for token in variant.split('_') {
            match token {
                "o" | "b" | "c" => whole += 1,
                "d" | "cd" => gradients = gradient,
                "l" | "cl" => packed += 1,
                _ => {}
            }
        }
    } else if name.contains("_mip") {
        packed += 1;
    }
    if a16 {
        whole + 2 * gradients.div_ceil(2) + packed.div_ceil(2)
    } else {
        whole + 2 * gradients + packed
    }
}

/// Whether an image operation takes a sampler.
pub(super) fn image_has_sampler(name: &str) -> bool {
    name.starts_with("image_sample")
        || name.starts_with("image_gather4")
        || name.starts_with("image_get_lod")
}
