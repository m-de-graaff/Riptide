//! Tests of the decoder and the printer on hand-encoded instructions.
//!
//! Each word below was assembled by hand from the public field layouts, or
//! is an encoding quoted in the tests of another decoder of this
//! instruction set. The expected text is what LLVM's assembler syntax gives
//! for that instruction.

use super::opcodes::{self, Op};
use super::{
    Detail, Format, Instruction, LanePermutation, Operand, Role, WaveSize, decode_at,
    decode_linear, listing, walk,
};

/// Decodes the one instruction that `words` holds and checks that it uses
/// all of them.
fn one(words: &[u32], wave: WaveSize) -> Instruction {
    let instruction = decode_at(words, 0, wave).unwrap();
    assert_eq!(instruction.words, words, "{instruction}");
    assert!(!instruction.truncated);
    instruction
}

/// The text of the one instruction that `words` holds, for 64 lanes.
fn text(words: &[u32]) -> String {
    one(words, WaveSize::Wave64).to_string()
}

/// The text of the one instruction that `words` holds, for 32 lanes.
fn text32(words: &[u32]) -> String {
    one(words, WaveSize::Wave32).to_string()
}

#[test]
fn opcode_tables_are_sorted_and_named_once() {
    let tables: [(&str, &[Op]); 19] = [
        ("SOP2", opcodes::SOP2),
        ("SOPK", opcodes::SOPK),
        ("SOP1", opcodes::SOP1),
        ("SOPC", opcodes::SOPC),
        ("SOPP", opcodes::SOPP),
        ("SMEM", opcodes::SMEM),
        ("VOP2", opcodes::VOP2),
        ("VOP1", opcodes::VOP1),
        ("VOPC", opcodes::VOPC),
        ("VOP3", opcodes::VOP3),
        ("VOP3P", opcodes::VOP3P),
        ("VINTRP", opcodes::VINTRP),
        ("DS", opcodes::DS),
        ("MUBUF", opcodes::MUBUF),
        ("MTBUF", opcodes::MTBUF),
        ("MIMG", opcodes::MIMG),
        ("FLAT", opcodes::FLAT),
        ("GLOBAL", opcodes::GLOBAL),
        ("SCRATCH", opcodes::SCRATCH),
    ];
    let mut total = 0;
    for (table_name, table) in tables {
        assert!(
            table.windows(2).all(|pair| pair[0].code < pair[1].code),
            "{table_name} is not sorted"
        );
        let mut names: Vec<&str> = table.iter().map(|entry| entry.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), table.len(), "{table_name} repeats a name");
        total += table.len();
    }
    assert_eq!(total, 1232);
    // The three-source table holds only opcodes outside the ranges that
    // stand for the shorter formats.
    assert!(opcodes::VOP3.iter().all(|entry| {
        entry.code >= opcodes::VOP3_OWN_BASE
            && !(opcodes::VOP3_VOP1_BASE..opcodes::VOP3_VOP1_END).contains(&entry.code)
    }));
}

#[test]
fn scalar_two_source() {
    assert_eq!(text(&[0x8782_027E]), "s_and_b64 s[2:3], exec, s[2:3]");
    assert_eq!(text(&[0x8008_FF08, 0x10]), "s_add_u32 s8, s8, 0x10");
    assert_eq!(text(&[0x8F84_8102]), "s_lshl_b64 s[4:5], s[2:3], 1");
    assert_eq!(text(&[0x8209_C109]), "s_addc_u32 s9, s9, -1");
    // s_mul_i32 with vcc_lo, m0 and a float constant as operands.
    assert_eq!(text(&[0x936A_F27C]), "s_mul_i32 vcc_lo, m0, 1.0");

    let instruction = one(&[0x8782_027E], WaveSize::Wave64);
    assert_eq!(instruction.format, Format::Sop2);
    assert_eq!(instruction.opcode, 0x0F);
    assert_eq!(instruction.mnemonic, Some("s_and_b64"));
    assert_eq!(
        instruction.arg(Role::Result),
        Some(Operand::Scalar { first: 2, count: 2 })
    );
    assert_eq!(instruction.sources().count(), 2);
    assert_eq!(instruction.detail, Detail::None);
}

#[test]
fn scalar_immediate() {
    assert_eq!(text(&[0xB005_3141]), "s_movk_i32 s5, 0x3141");
    assert_eq!(text(&[0xB182_0010]), "s_cmpk_eq_i32 s2, 0x10");
    assert_eq!(
        text(&[0xBA80_0801, 0x0000_0001]),
        "s_setreg_imm32_b32 0x801, 0x1"
    );
    assert_eq!(text(&[0xBB0A_0004]), "s_call_b64 s[10:11], 4");
    let call = one(&[0xBB0A_0004], WaveSize::Wave64);
    assert_eq!(call.format, Format::Sopk);
    assert_eq!(call.branch_target(), Some(4 + 16));
    let compare = one(&[0xB182_0010], WaveSize::Wave64);
    assert_eq!(compare.arg(Role::Result), None);
    assert_eq!(compare.detail, Detail::Immediate(0x10));
}

#[test]
fn scalar_one_source() {
    assert_eq!(
        text(&[0xBE80_03FF, 0x1234_5678]),
        "s_mov_b32 s0, 0x12345678"
    );
    assert_eq!(text(&[0xBE80_047E]), "s_mov_b64 s[0:1], exec");
    assert_eq!(text(&[0xBE88_1F00]), "s_getpc_b64 s[8:9]");
    assert_eq!(text(&[0xBE80_2008]), "s_setpc_b64 s[8:9]");
    assert_eq!(text(&[0xBE84_246A]), "s_and_saveexec_b64 s[4:5], vcc");
    // The 32-bit form of the same, as programs of 32 lanes use it.
    assert_eq!(text32(&[0xBE84_3C6A]), "s_and_saveexec_b32 s4, vcc_lo");
    assert_eq!(text(&[0xBE85_1004]), "s_bcnt1_i32_b64 s5, s[4:5]");
}

#[test]
fn scalar_compare() {
    assert_eq!(text(&[0xBF00_8100]), "s_cmp_eq_i32 s0, 1");
    assert_eq!(text(&[0xBF13_8004]), "s_cmp_lg_u64 s[4:5], 0");
    assert_eq!(
        text(&[0xBF06_FF03, 0xDEAD_BEEF]),
        "s_cmp_eq_u32 s3, 0xdeadbeef"
    );
    assert_eq!(one(&[0xBF00_8100], WaveSize::Wave64).format, Format::Sopc);
}

#[test]
fn program_control() {
    assert_eq!(text(&[0xBF81_0000]), "s_endpgm");
    assert_eq!(text(&[0xBF80_0000]), "s_nop 0x0");
    assert_eq!(text(&[0xBF88_0001]), "s_cbranch_execz 1");
    assert_eq!(text(&[0xBF82_FFFE]), "s_branch -2");
    // The export counter is at its greatest value, so it is not waited for.
    assert_eq!(text(&[0xBF8C_0070]), "s_waitcnt vmcnt(0) lgkmcnt(0)");
    assert_eq!(
        text(&[0xBF8C_CF7F]),
        "s_waitcnt vmcnt(63) expcnt(7) lgkmcnt(15)"
    );
    assert_eq!(text(&[0xBF90_0009]), "s_sendmsg sendmsg(MSG_GS_ALLOC_REQ)");
    assert_eq!(
        text(&[0xBF90_0122]),
        "s_sendmsg sendmsg(MSG_GS, GS_OP_EMIT, 1)"
    );
    assert_eq!(text(&[0xBFA8_1234]), "s_ttracedata_imm 0x1234");
    assert_eq!(text(&[0xBF9F_0000]), "s_code_end");

    let branch = one(&[0xBF82_FFFE], WaveSize::Wave64);
    assert_eq!(branch.format, Format::Sopp);
    assert_eq!(branch.branch_target(), Some(4 - 8));
    assert!(branch.ends_block());
    assert!(!one(&[0xBF88_0001], WaveSize::Wave64).ends_block());
    assert!(one(&[0xBF81_0000], WaveSize::Wave64).ends_block());
}

#[test]
fn scalar_memory() {
    assert_eq!(
        text(&[0xF408_0501, 0xFA00_0040]),
        "s_load_dwordx4 s[20:23], s[2:3], 0x40"
    );
    assert_eq!(
        text(&[0xF420_0144, 0x0400_0000]),
        "s_buffer_load_dword s5, s[8:11], s2"
    );
    assert_eq!(
        text(&[0xF421_0144, 0x0400_0010]),
        "s_buffer_load_dword s5, s[8:11], s2 offset:0x10 glc"
    );
    assert_eq!(text(&[0xF490_0500, 0]), "s_memtime s[20:21]");
    assert_eq!(text(&[0xF4A8_0140, 0]), "s_get_waveid_in_workgroup s5");
    assert_eq!(text(&[0xF480_0000, 0]), "s_dcache_inv");
    // The data field of a probe is a number, not a register.
    assert_eq!(
        text(&[0xF498_01C2, 0xFA00_0064]),
        "s_atc_probe 0x7, s[4:5], 0x64"
    );

    let load = one(&[0xF408_0501, 0xFA00_0040], WaveSize::Wave64);
    assert_eq!(load.format, Format::Smem);
    assert_eq!(
        load.arg(Role::Base),
        Some(Operand::Scalar { first: 2, count: 2 })
    );
    assert_eq!(load.arg(Role::Offset), None);
    assert_eq!(
        load.detail,
        Detail::ScalarMemory {
            offset: 0x40,
            glc: false,
            dlc: false
        }
    );
}

#[test]
fn vector_two_source() {
    assert_eq!(text(&[0x0600_0000]), "v_add_f32 v0, s0, v0");
    assert_eq!(
        text(&[0x1004_0AFF, 0x3F00_0001]),
        "v_mul_f32 v2, 0x3f000001, v5"
    );
    assert_eq!(text32(&[0x0202_0480]), "v_cndmask_b32 v1, 0, v2, vcc_lo");
    assert_eq!(text(&[0x0202_0480]), "v_cndmask_b32 v1, 0, v2, vcc");
    assert_eq!(text(&[0x5006_0801]), "v_add_co_ci_u32 v3, vcc, s1, v4, vcc");
    assert_eq!(
        text(&[0x4202_0702, 0x4000_0000]),
        "v_madak_f32 v1, v2, v3, 0x40000000"
    );
    assert_eq!(
        text(&[0x4002_0702, 0x4000_0000]),
        "v_madmk_f32 v1, v2, 0x40000000, v3"
    );
    assert_eq!(one(&[0x0600_0000], WaveSize::Wave64).format, Format::Vop2);
}

#[test]
fn vector_one_source() {
    assert_eq!(text(&[0x7E02_0302]), "v_mov_b32 v1, v2");
    assert_eq!(text(&[0x7E06_0504]), "v_readfirstlane_b32 s3, v4");
    assert_eq!(text(&[0x7E04_0901]), "v_cvt_f64_i32 v[2:3], v1");
    assert_eq!(text(&[0x7E02_02F2]), "v_mov_b32 v1, 1.0");
    // The fields of a no-operation are not operands, whatever they hold. A
    // source field that asks for a literal still makes it two words long.
    assert_eq!(text(&[0x7FFE_0101]), "v_nop");
    assert_eq!(text(&[0x7E00_00FF, 0xDEAD_BEEF]), "v_nop");
    assert_eq!(one(&[0x7E02_0302], WaveSize::Wave64).format, Format::Vop1);
}

#[test]
fn vector_sub_register_and_lane_permutation() {
    assert_eq!(
        text(&[0x7E02_16F9, 0x0605_0602]),
        "v_cvt_f32_f16_sdwa v1, v2 dst_sel:DWORD dst_unused:UNUSED_PAD src0_sel:WORD_1"
    );
    // A two-source operation whose second source is sign-extended byte 0 of
    // a scalar register.
    assert_eq!(
        text(&[0x0602_08F9, 0x8806_1402]),
        "v_add_f32_sdwa v1, v2, sext(s4) dst_sel:WORD_0 dst_unused:UNUSED_PRESERVE \
         src0_sel:DWORD src1_sel:BYTE_0"
    );
    assert_eq!(
        text(&[0x7E0A_02FA, 0xFF08_B106]),
        "v_mov_b32_dpp v5, v6 quad_perm:[1,0,3,2] row_mask:0xf bank_mask:0xf bound_ctrl:1"
    );
    assert_eq!(
        text(&[0x7E0A_02FA, 0xA105_0106]),
        "v_mov_b32_dpp v5, v6 row_shl:1 row_mask:0xa bank_mask:0x1 fi:1"
    );
    assert_eq!(
        text(&[0x7E06_02E9, 0x0539_7701]),
        "v_mov_b32_dpp v3, v1 dpp8:[7,6,5,4,3,2,1,0]"
    );
    assert_eq!(
        text(&[0x7E06_02EA, 0x0539_7701]),
        "v_mov_b32_dpp v3, v1 dpp8:[7,6,5,4,3,2,1,0] fi:1"
    );

    let permuted = one(&[0x7E0A_02FA, 0xFF08_B106], WaveSize::Wave64);
    let Detail::Vector(modifiers) = permuted.detail else {
        panic!("not a vector operation");
    };
    assert_eq!(
        modifiers.permutation,
        Some(LanePermutation::Rows {
            control: 0xB1,
            fetch_inactive: false,
            bound_control: true,
            bank_mask: 0xF,
            row_mask: 0xF
        })
    );
}

#[test]
fn vector_compare() {
    assert_eq!(text32(&[0x7C08_06F0]), "v_cmp_gt_f32 vcc_lo, 0.5, v3");
    assert_eq!(text(&[0x7C08_06F0]), "v_cmp_gt_f32 vcc, 0.5, v3");
    // The comparisons that write EXEC name no result.
    assert_eq!(text(&[0x7C22_0501]), "v_cmpx_lt_f32 v1, v2");
    assert_eq!(text(&[0x7D42_0902]), "v_cmp_lt_i64 vcc, v[2:3], v[4:5]");
    assert_eq!(one(&[0x7C08_06F0], WaveSize::Wave64).format, Format::Vopc);
}

#[test]
fn vector_three_source() {
    assert_eq!(
        text32(&[0xD401_0106, 0x0001_E501]),
        "v_cmp_lt_f32_e64 s6, |v1|, 1.0"
    );
    assert_eq!(
        text(&[0xD401_0106, 0x0001_E501]),
        "v_cmp_lt_f32_e64 s[6:7], |v1|, 1.0"
    );
    assert_eq!(
        text(&[0xD54B_8004, 0x2BC0_0501]),
        "v_fma_f32 v4, -v1, s2, 0.5 clamp mul:2"
    );
    assert_eq!(
        text(&[0xD54B_0000, 0x03FE_0000, 0x4000_0000]),
        "v_fma_f32 v0, s0, v0, 0x40000000"
    );
    assert_eq!(
        text(&[0xD564_0000, 0x0002_0902]),
        "v_add_f64 v[0:1], v[2:3], v[4:5]"
    );
    // The long encodings of a one-source and a two-source operation.
    assert_eq!(text(&[0xD581_0001, 0x0000_0102]), "v_mov_b32_e64 v1, v2");
    assert_eq!(
        text(&[0xD503_0001, 0x4002_0702]),
        "v_add_f32_e64 v1, v2, -v3"
    );
    assert_eq!(
        text(&[0xD501_0005, 0x0012_0702]),
        "v_cndmask_b32_e64 v5, v2, v3, s[4:5]"
    );
    assert_eq!(
        text(&[0xD760_0003, 0x0000_0501]),
        "v_readlane_b32 s3, v1, s2"
    );
    // A 16-bit operation that reads the high half of its second source and
    // writes the high half of its result.
    assert_eq!(
        text(&[0xD74B_5001, 0x0412_0702]),
        "v_fma_f16 v1, v2, v3, v4 op_sel:[0,1,0,1]"
    );

    let fused = one(&[0xD54B_8004, 0x2BC0_0501], WaveSize::Wave64);
    assert_eq!(fused.format, Format::Vop3a);
    assert_eq!(fused.opcode, 0x14B);
    assert!(!fused.is_long_form());
    assert!(one(&[0xD581_0001, 0x0000_0102], WaveSize::Wave64).is_long_form());
}

#[test]
fn vector_three_source_with_second_result() {
    assert_eq!(
        text(&[0xD70F_6A01, 0x0002_0702]),
        "v_add_co_u32 v1, vcc, v2, v3"
    );
    assert_eq!(
        text32(&[0xD70F_0401, 0x0002_0702]),
        "v_add_co_u32 v1, s4, v2, v3"
    );
    assert_eq!(
        text(&[0xD576_0200, 0x0412_0702]),
        "v_mad_u64_u32 v[0:1], s[2:3], v2, v3, v[4:5]"
    );
    assert_eq!(
        text(&[0xD528_0605, 0x0022_0702]),
        "v_add_co_ci_u32_e64 v5, s[6:7], v2, v3, s[8:9]"
    );
    let carry = one(&[0xD70F_6A01, 0x0002_0702], WaveSize::Wave64);
    assert_eq!(carry.format, Format::Vop3b);
    assert_eq!(
        carry.arg(Role::SecondResult),
        Some(Operand::Vcc(super::Half::Both))
    );
}

#[test]
fn vector_packed() {
    assert_eq!(text(&[0xCC0F_4001, 0x1A02_0702]), "v_pk_add_f16 v1, v2, v3");
    assert_eq!(
        text(&[0xCC20_0C01, 0x2C10_0702]),
        "v_fma_mix_f32 v1, v2, s3, v4 op_sel:[1,0,0] op_sel_hi:[1,0,0] neg_lo:[1,0,0] \
         neg_hi:[0,0,1]"
    );
    assert_eq!(
        text(&[0xCC0E_C001, 0x1C12_0702]),
        "v_pk_fma_f16 v1, v2, v3, v4 clamp"
    );
    assert_eq!(
        one(&[0xCC0F_4001, 0x1A02_0702], WaveSize::Wave64).format,
        Format::Vop3p
    );
}

#[test]
fn interpolation() {
    assert_eq!(text(&[0xC804_0002]), "v_interp_p1_f32 v1, v2, attr0.x");
    assert_eq!(text(&[0xC80E_1702]), "v_interp_mov_f32 v3, p0, attr5.w");
    let instruction = one(&[0xC80E_1702], WaveSize::Wave64);
    assert_eq!(instruction.format, Format::Vintrp);
    assert_eq!(
        instruction.detail,
        Detail::Interpolation {
            attribute: 5,
            channel: 3
        }
    );
}

#[test]
fn group_shared_memory() {
    assert_eq!(
        text(&[0xD8DC_0302, 0x0600_0001]),
        "ds_read2_b32 v[6:7], v1 offset0:2 offset1:3"
    );
    assert_eq!(
        text(&[0xD834_0004, 0x0000_0201]),
        "ds_write_b32 v1, v2 offset:4"
    );
    assert_eq!(text(&[0xD9D8_0000, 0x0400_0001]), "ds_read_b64 v[4:5], v1");
    assert_eq!(
        text(&[0xD880_0100, 0x0504_0302]),
        "ds_add_rtn_u32 v5, v2, v3 offset:256"
    );
    assert_eq!(text(&[0xD840_0000, 0x0004_0302]), "ds_cmpst_b32 v2, v3, v4");
    assert_eq!(text(&[0xD850_0000, 0]), "ds_nop");
    let read = one(&[0xD8DC_0302, 0x0600_0001], WaveSize::Wave64);
    assert_eq!(read.format, Format::Ds);
    assert_eq!(
        read.arg(Role::Address),
        Some(Operand::Vector { first: 1, count: 1 })
    );
}

#[test]
fn buffers() {
    assert_eq!(
        text(&[0xE034_7010, 0x0102_0402]),
        "buffer_load_dwordx2 v[4:5], v[2:3], s[8:11], s1 idxen offen offset:16 glc"
    );
    assert_eq!(
        text(&[0xE031_1000, 0x0001_0001]),
        "buffer_load_dword v0, v1, s[4:7], s0 offen"
    );
    assert_eq!(
        text(&[0xE070_0008, 0x8000_0300]),
        "buffer_store_dword v3, off, s[0:3], 0 offset:8"
    );
    // The opcode's eighth bit sits apart from the others.
    assert_eq!(
        text(&[0xE200_2000, 0x0001_0201]),
        "buffer_load_format_d16_x v2, v1, s[4:7], s0 idxen"
    );
    // A load that also returns the texture-fail word is one register wider.
    assert_eq!(
        text(&[0xE030_2000, 0x0081_0201]),
        "buffer_load_dword v[2:3], v1, s[4:7], s0 idxen tfe"
    );
    assert_eq!(
        text(&[0xE8B3_2000, 0x8002_0401]),
        "tbuffer_load_format_xyzw v[4:7], v1, s[8:11], format:22, 0 idxen"
    );
    // The typed opcode's fourth bit is in the second word.
    assert_eq!(
        text(&[0xE8B0_2000, 0x8022_0401]),
        "tbuffer_load_format_d16_x v4, v1, s[8:11], format:22, 0 idxen"
    );
    let load = one(&[0xE034_7010, 0x0102_0402], WaveSize::Wave64);
    assert_eq!(load.format, Format::Mubuf);
    assert_eq!(
        load.arg(Role::Resource),
        Some(Operand::Scalar { first: 8, count: 4 })
    );
    assert_eq!(
        one(&[0xE8B3_2000, 0x8002_0401], WaveSize::Wave64).format,
        Format::Mtbuf
    );
}

#[test]
fn images() {
    assert_eq!(
        text(&[0xF080_0F08, 0x0062_0501]),
        "image_sample v[5:8], v[1:2], s[8:15], s[12:15] dmask:0xf dim:SQ_RSRC_IMG_2D"
    );
    // Address registers that are not consecutive, and 16-bit data: three
    // components in two registers.
    assert_eq!(
        text(&[0xF09C_070A, 0x8024_0801, 0x0000_0005]),
        "image_sample_lz v[8:9], [v1, v5], s[16:23], s[4:7] dmask:0x7 dim:SQ_RSRC_IMG_2D d16"
    );
    // The opcode's eighth bit is bit 0 of the first word.
    assert_eq!(
        text(&[0xF19C_0F01, 0x0001_0004]),
        "image_bvh64_intersect_ray v[0:3], v[4:15], s[4:11] dmask:0xf dim:SQ_RSRC_IMG_1D"
    );
    assert_eq!(
        text(&[0xF020_1128, 0x0003_0201]),
        "image_store v2, v[1:3], s[12:19] dmask:0x1 dim:SQ_RSRC_IMG_2D_ARRAY unorm"
    );
    // A cube sample with a bias and a comparison value: one register each,
    // then three coordinates.
    assert_eq!(
        text(&[0xF0B4_0118, 0x0022_0001]),
        "image_sample_c_b v0, v[1:5], s[8:15], s[4:7] dmask:0x1 dim:SQ_RSRC_IMG_CUBE"
    );
    let sample = one(&[0xF09C_070A, 0x8024_0801, 0x0000_0005], WaveSize::Wave64);
    assert_eq!(sample.format, Format::Mimg);
    assert_eq!(
        sample.arg(Role::Sampler),
        Some(Operand::Scalar { first: 4, count: 4 })
    );
    assert_eq!(
        sample
            .args
            .iter()
            .filter(|arg| arg.role == Role::Address)
            .count(),
        2
    );
}

#[test]
fn memory_by_address() {
    assert_eq!(
        text(&[0xDC30_8000, 0x007D_0002]),
        "global_load_dword v0, v[2:3], off"
    );
    assert_eq!(
        text(&[0xDC30_8FFC, 0x0102_0003]),
        "global_load_dword v1, v3, s[2:3] offset:-4"
    );
    assert_eq!(
        text(&[0xDC70_0010, 0x007D_0103]),
        "flat_store_dword v[3:4], v1 offset:16"
    );
    assert_eq!(
        text(&[0xDC30_4000, 0x017D_0002]),
        "scratch_load_dword v1, v2, off"
    );
    assert_eq!(
        text(&[0xDC30_4008, 0x0105_0000]),
        "scratch_load_dword v1, off, s5 offset:8"
    );
    // An atomic operation returns the old value only with GLC set.
    assert_eq!(
        text(&[0xDCC8_0000, 0x007D_0301]),
        "flat_atomic_add v[1:2], v3"
    );
    assert_eq!(
        text(&[0xDCC9_0000, 0x057D_0301]),
        "flat_atomic_add v5, v[1:2], v3 glc"
    );

    // The top bit of a flat offset is ignored; the other two formats take
    // all twelve bits as a signed number.
    let offset = |words: &[u32]| match one(words, WaveSize::Wave64).detail {
        Detail::Flat { offset, .. } => offset,
        other => panic!("{other:?}"),
    };
    assert_eq!(offset(&[0xDC30_0FFF, 0x007D_0002]), 0x7FF);
    assert_eq!(offset(&[0xDC30_0800, 0x007D_0002]), 0);
    assert_eq!(offset(&[0xDC30_8FFF, 0x007D_0002]), -1);
    assert_eq!(
        one(&[0xDC30_8000, 0x007D_0002], WaveSize::Wave64).format,
        Format::Global
    );
    assert_eq!(
        one(&[0xDC30_4000, 0x017D_0002], WaveSize::Wave64).format,
        Format::Scratch
    );
    // The fourth address space does not exist.
    let reserved = one(&[0xDC30_C000, 0], WaveSize::Wave64);
    assert_eq!((reserved.format, reserved.mnemonic), (Format::Flat, None));
}

#[test]
fn exports() {
    assert_eq!(
        text(&[0xF800_080F, 0x0302_0100]),
        "exp mrt0 v0, v1, v2, v3 done"
    );
    assert_eq!(
        text(&[0xF800_1C0F, 0x0000_0302]),
        "exp mrt0 v2, v2, v3, v3 done compr vm"
    );
    assert_eq!(
        text(&[0xF800_00C7, 0x0002_0100]),
        "exp pos0 v0, v1, v2, off"
    );
    assert_eq!(
        text(&[0xF800_0201, 0x0000_0007]),
        "exp param0 v7, off, off, off"
    );
    assert_eq!(text(&[0xF800_0140, 0]), "exp prim off, off, off, off");
    let export = one(&[0xF800_080F, 0x0302_0100], WaveSize::Wave64);
    assert_eq!(export.format, Format::Exp);
    assert_eq!(export.mnemonic, Some("exp"));
    assert_eq!(
        export.detail,
        Detail::Export {
            target: 0,
            enabled: 0xF,
            compressed: false,
            done: true,
            valid_mask: false
        }
    );
}

#[test]
fn unknown_instructions_keep_their_words() {
    // Opcode 0x0C of the two-source scalar format is not assigned.
    let instruction = one(&[0x8600_0000], WaveSize::Wave64);
    assert!(instruction.is_unknown());
    assert_eq!(
        (instruction.format, instruction.opcode),
        (Format::Sop2, 0x0C)
    );
    assert!(instruction.args.is_empty());
    assert_eq!(
        instruction.to_string(),
        ".long 0x86000000 ; unknown SOP2 opcode 0xc"
    );

    // An unassigned three-source opcode still has two words, or three when
    // a source field asks for a literal.
    let instruction = one(&[0xD7FF_0000, 0x0000_0102], WaveSize::Wave64);
    assert_eq!(
        (instruction.format, instruction.opcode, instruction.mnemonic),
        (Format::Vop3a, 0x3FF, None)
    );
    assert_eq!(
        instruction.to_string(),
        ".long 0xd7ff0000, 0x00000102 ; unknown VOP3A opcode 0x3ff"
    );
    assert_eq!(
        one(&[0xD7FF_0000, 0x0000_00FF, 7], WaveSize::Wave64)
            .words
            .len(),
        3
    );

    // Top bits that select no format.
    for word in [0xC000_0000u32, 0xD000_0000, 0xE400_0000, 0xFC00_0000] {
        let instruction = one(&[word], WaveSize::Wave64);
        assert_eq!(
            (instruction.format, instruction.mnemonic),
            (Format::Unknown, None)
        );
    }
    assert_eq!(text(&[0xC000_0000]), ".long 0xc0000000 ; unknown format");
}

#[test]
fn an_instruction_cut_off_by_the_end_is_reported() {
    for words in [
        &[0xBE80_03FFu32][..],
        &[0xD54B_0000],
        &[0xD54B_0000, 0x03FE_0000],
        &[0xF09C_070A, 0x8024_0801],
        &[0xF800_080F],
    ] {
        let instruction = decode_at(words, 0, WaveSize::Wave64).unwrap();
        assert!(instruction.truncated, "{words:08x?}");
        assert!(instruction.is_unknown());
        assert_eq!(instruction.words, words);
    }
    assert_eq!(
        decode_at(&[0xD54B_0000], 0, WaveSize::Wave64)
            .unwrap()
            .to_string(),
        ".long 0xd54b0000 ; VOP3A instruction cut off by the end"
    );
    assert_eq!(decode_at(&[], 0, WaveSize::Wave64), None);
    assert_eq!(decode_at(&[0xBF81_0000], 1, WaveSize::Wave64), None);
    assert_eq!(
        decode_at(&[0xBF81_0000], usize::MAX, WaveSize::Wave64),
        None
    );
}

/// Builds the first two words of an instruction of a format with a given
/// opcode, all other bits taken from `fill`.
fn encode(format: Format, opcode: u16, fill: (u32, u32)) -> [u32; 2] {
    let opcode = u32::from(opcode);
    let (mut first, mut second) = fill;
    // Clears a field of the first word and sets it.
    let mut set = |low: u32, width: u32, value: u32| {
        let mask = ((1u64 << width) - 1) as u32;
        first = first & !(mask << low) | (value & mask) << low;
    };
    match format {
        Format::Sop2 => {
            set(30, 2, 0b10);
            set(23, 7, opcode);
        }
        Format::Sopk => {
            set(28, 4, 0b1011);
            set(23, 5, opcode);
        }
        Format::Sop1 => {
            set(23, 9, 0x17D);
            set(8, 8, opcode);
        }
        Format::Sopc => {
            set(23, 9, 0x17E);
            set(16, 7, opcode);
        }
        Format::Sopp => {
            set(23, 9, 0x17F);
            set(16, 7, opcode);
        }
        Format::Vop2 => {
            set(31, 1, 0);
            set(25, 6, opcode);
        }
        Format::Vop1 => {
            set(25, 7, 0x3F);
            set(9, 8, opcode);
        }
        Format::Vopc => {
            set(25, 7, 0x3E);
            set(17, 8, opcode);
        }
        Format::Vop3a | Format::Vop3b => {
            set(26, 6, 0x35);
            set(16, 10, opcode);
        }
        Format::Vop3p => {
            set(26, 6, 0x33);
            set(16, 7, opcode);
        }
        Format::Vintrp => {
            set(26, 6, 0x32);
            set(16, 2, opcode);
        }
        Format::Ds => {
            set(26, 6, 0x36);
            set(18, 8, opcode);
        }
        Format::Flat | Format::Scratch | Format::Global => {
            set(26, 6, 0x37);
            set(18, 7, opcode);
            let space = match format {
                Format::Flat => 0,
                Format::Scratch => 1,
                _ => 2,
            };
            set(14, 2, space);
        }
        Format::Mubuf => {
            set(26, 6, 0x38);
            set(18, 7, opcode);
            set(25, 1, opcode >> 7);
        }
        Format::Mtbuf => {
            set(26, 6, 0x3A);
            set(16, 3, opcode);
            second = second & !(1 << 21) | (opcode >> 3 & 1) << 21;
        }
        Format::Mimg => {
            set(26, 6, 0x3C);
            set(18, 7, opcode);
            set(0, 1, opcode >> 7);
        }
        Format::Smem => {
            set(26, 6, 0x3D);
            set(18, 8, opcode);
        }
        Format::Exp => set(26, 6, 0x3E),
        Format::Unknown => set(26, 6, 0x30),
    }
    [first, second]
}

/// A small deterministic generator of pseudo-random words (xorshift).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
}

#[test]
fn every_opcode_of_every_table_decodes_to_its_name() {
    let tables: [(Format, &[Op], u16); 21] = [
        (Format::Sop2, opcodes::SOP2, 0),
        (Format::Sopk, opcodes::SOPK, 0),
        (Format::Sop1, opcodes::SOP1, 0),
        (Format::Sopc, opcodes::SOPC, 0),
        (Format::Sopp, opcodes::SOPP, 0),
        (Format::Smem, opcodes::SMEM, 0),
        (Format::Vop2, opcodes::VOP2, 0),
        (Format::Vop1, opcodes::VOP1, 0),
        (Format::Vopc, opcodes::VOPC, 0),
        (Format::Vop3a, opcodes::VOP3, 0),
        // The shorter formats inside the three-source encoding.
        (Format::Vop3a, opcodes::VOPC, 0),
        (Format::Vop3a, opcodes::VOP2, opcodes::VOP3_VOP2_BASE),
        (Format::Vop3a, opcodes::VOP1, opcodes::VOP3_VOP1_BASE),
        (Format::Vop3p, opcodes::VOP3P, 0),
        (Format::Vintrp, opcodes::VINTRP, 0),
        (Format::Ds, opcodes::DS, 0),
        (Format::Mubuf, opcodes::MUBUF, 0),
        (Format::Mtbuf, opcodes::MTBUF, 0),
        (Format::Mimg, opcodes::MIMG, 0),
        (Format::Flat, opcodes::FLAT, 0),
        (Format::Global, opcodes::GLOBAL, 0),
    ];
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let mut checked = 0;
    for (format, table, base) in tables {
        for entry in table {
            for round in 0..48 {
                // The first rounds use all-zero and all-one fields.
                let fill = match round {
                    0 => (0, 0),
                    1 => (u32::MAX, u32::MAX),
                    _ => (random.next(), random.next()),
                };
                let [first, second] = encode(format, entry.code + base, fill);
                let code = [first, second, random.next(), random.next(), random.next()];
                for wave in [WaveSize::Wave32, WaveSize::Wave64] {
                    let instruction = decode_at(&code, 0, wave).unwrap();
                    assert_eq!(
                        instruction.mnemonic,
                        Some(entry.name),
                        "{first:08x} {second:08x}"
                    );
                    let same_format = match format {
                        Format::Vop3a => {
                            matches!(instruction.format, Format::Vop3a | Format::Vop3b)
                        }
                        other => instruction.format == other,
                    };
                    assert!(same_format, "{first:08x}: {:?}", instruction.format);
                    assert_eq!(instruction.opcode, entry.code + base);
                    assert!(instruction.to_string().starts_with(entry.name));
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 100_000);
    // The scratch table, whose format the list above leaves out to keep its
    // type simple.
    for entry in opcodes::SCRATCH {
        let [first, second] = encode(Format::Scratch, entry.code, (0, 0));
        let instruction = decode_at(&[first, second], 0, WaveSize::Wave64).unwrap();
        assert_eq!(instruction.mnemonic, Some(entry.name));
        assert_eq!(instruction.format, Format::Scratch);
    }
}

#[test]
fn a_million_random_words_decode_without_panicking() {
    const WORDS: usize = 1 << 20;
    let mut random = Random(0x0123_4567_89AB_CDEF);
    let code: Vec<u32> = (0..WORDS).map(|_| random.next()).collect();
    let mut formats = std::collections::BTreeSet::new();
    let mut named = 0usize;
    let mut text_length = 0usize;
    for index in 0..WORDS {
        let instruction = decode_at(&code, index, WaveSize::Wave64).unwrap();
        assert_eq!(instruction.offset as usize, 4 * index);
        assert!(!instruction.words.is_empty() && instruction.words.len() <= 5);
        assert_eq!(instruction.words[0], code[index]);
        if instruction.is_unknown() {
            assert!(instruction.args.is_empty());
        }
        formats.insert(instruction.format);
        named += usize::from(!instruction.is_unknown());
        text_length += instruction.to_string().len();
        // Every eighth word is also decoded for the other wave size.
        if index % 8 == 0 {
            let other = decode_at(&code, index, WaveSize::Wave32).unwrap();
            assert_eq!(other.words, instruction.words);
            assert_eq!(other.mnemonic, instruction.mnemonic);
            text_length += other.to_string().len();
        }
    }
    // Random words reach every format, and most of them name an instruction.
    assert_eq!(formats.len(), 22);
    assert!(named > WORDS / 2, "{named}");
    assert!(text_length > WORDS);

    // The whole-program decoders survive the same words.
    let linear = decode_linear(&code, WaveSize::Wave64);
    let covered: usize = linear.iter().map(|i| i.words.len()).sum();
    assert_eq!(covered, WORDS);
    let walked = walk(&code, WaveSize::Wave32);
    let reached: usize = walked.instructions.iter().map(|i| i.words.len()).sum();
    assert_eq!(reached + walked.unreached_words, WORDS);
    assert!(!listing(&walked.instructions).is_empty());
}

#[test]
fn linear_decode_steps_over_literals() {
    let code = [
        0xBE80_03FF,
        0xBF81_0000, // a literal that happens to look like s_endpgm
        0x0600_0000,
        0xBF81_0000,
    ];
    let instructions = decode_linear(&code, WaveSize::Wave64);
    let names: Vec<&str> = instructions.iter().filter_map(|i| i.mnemonic).collect();
    assert_eq!(names, ["s_mov_b32", "v_add_f32", "s_endpgm"]);
    let offsets: Vec<u32> = instructions.iter().map(|i| i.offset).collect();
    assert_eq!(offsets, [0, 8, 12]);
    assert!(decode_linear(&[], WaveSize::Wave64).is_empty());
}

#[test]
fn walk_follows_branches_and_leaves_data_alone() {
    let code = [
        0xBF88_0002, // s_cbranch_execz 2 -> the end
        0xBE80_03FF, // s_mov_b32 s0, <literal>
        0x1234_5678,
        0xBF81_0000, // s_endpgm
        0xFFFF_FFFF, // data
        0xFFFF_FFFF,
    ];
    let walked = walk(&code, WaveSize::Wave64);
    let offsets: Vec<u32> = walked.instructions.iter().map(|i| i.offset).collect();
    assert_eq!(offsets, [0, 4, 12]);
    assert_eq!(walked.unreached_words, 2);
    assert_eq!(walked.stray_branches, 0);
    assert!(!walked.runs_off_end);

    assert_eq!(
        listing(&walked.instructions),
        "0x000000  bf880002                   s_cbranch_execz 2 ; to 0x00000c\n\
         0x000004  be8003ff 12345678          s_mov_b32 s0, 0x12345678\n\
         0x00000c  bf810000                   s_endpgm\n"
    );
}

#[test]
fn walk_reaches_code_only_a_branch_leads_to() {
    let code = [
        0xBF82_0002, // s_branch 2 -> word 3
        0xFFFF_FFFF, // data
        0xFFFF_FFFF,
        0xBF84_FFFF, // s_cbranch_scc0 -1 -> itself
        0xBF81_0000, // s_endpgm
    ];
    let walked = walk(&code, WaveSize::Wave64);
    let offsets: Vec<u32> = walked.instructions.iter().map(|i| i.offset).collect();
    assert_eq!(offsets, [0, 12, 16]);
    assert_eq!(walked.unreached_words, 2);
    assert_eq!(walked.stray_branches, 0);
    assert_eq!(
        listing(&walked.instructions),
        "0x000000  bf820002                   s_branch 2 ; to 0x00000c\n\
         \x20         ; 2 words not decoded\n\
         0x00000c  bf84ffff                   s_cbranch_scc0 -1 ; to 0x00000c\n\
         0x000010  bf810000                   s_endpgm\n"
    );
}

#[test]
fn walk_counts_what_cannot_be_code() {
    // A branch into the literal of the move.
    let code = [0xBF84_0001, 0xBE80_03FF, 0xBF81_0000, 0xBF81_0000];
    let walked = walk(&code, WaveSize::Wave64);
    assert_eq!(walked.stray_branches, 1);
    assert_eq!(walked.instructions.len(), 3);

    // Branches outside the code, before and after it.
    for word in [0xBF82_0010u32, 0xBF82_FFF0] {
        let walked = walk(&[word, 0xBF81_0000], WaveSize::Wave64);
        assert_eq!(walked.stray_branches, 1);
        assert_eq!(walked.instructions.len(), 1);
        assert_eq!(walked.unreached_words, 1);
    }

    // Code that never ends, and code cut off inside an instruction.
    let walked = walk(&[0xBF80_0000], WaveSize::Wave64);
    assert!(walked.runs_off_end);
    let walked = walk(&[0xBE80_03FF], WaveSize::Wave64);
    assert!(walked.runs_off_end);
    assert!(walked.instructions[0].truncated);
    assert!(walk(&[], WaveSize::Wave64).runs_off_end);

    // An unknown instruction is stepped over.
    let walked = walk(&[0x8600_0000, 0xBF81_0000], WaveSize::Wave64);
    assert_eq!(walked.instructions.len(), 2);
    assert!(walked.instructions[0].is_unknown());
    assert!(!walked.runs_off_end);
}

#[test]
fn walk_follows_the_far_jump_sequence() {
    // Forward: the register gets the address after s_getpc_b64 (4), plus 20.
    let code = [
        0xBE88_1F00, // s_getpc_b64 s[8:9]
        0x8008_FF08, // s_add_u32 s8, s8, 20
        0x0000_0014,
        0x8209_8009, // s_addc_u32 s9, s9, 0
        0xBE80_2008, // s_setpc_b64 s[8:9]
        0xFFFF_FFFF, // data
        0xBF81_0000, // s_endpgm
    ];
    let walked = walk(&code, WaveSize::Wave64);
    let offsets: Vec<u32> = walked.instructions.iter().map(|i| i.offset).collect();
    assert_eq!(offsets, [0, 4, 12, 16, 24]);
    assert_eq!(walked.unreached_words, 1);
    assert_eq!(walked.stray_branches, 0);

    // Backward: 12 - 8 = 4.
    let code = [
        0xBF82_0001, // s_branch 1 -> word 2
        0xBF81_0000, // s_endpgm
        0xBE88_1F00, // s_getpc_b64 s[8:9]
        0x8008_FF08, // s_add_u32 s8, s8, -8
        0xFFFF_FFF8,
        0x8209_C109, // s_addc_u32 s9, s9, -1
        0xBE80_2008, // s_setpc_b64 s[8:9]
    ];
    let walked = walk(&code, WaveSize::Wave64);
    assert_eq!(walked.instructions.len(), 6);
    assert_eq!(walked.unreached_words, 0);
    assert_eq!(walked.stray_branches, 0);

    // A register jump that is not that sequence ends its path.
    let walked = walk(&[0xBE80_2018, 0xBF81_0000], WaveSize::Wave64);
    assert_eq!(walked.instructions.len(), 1);
    assert_eq!(walked.unreached_words, 1);
    assert_eq!(walked.stray_branches, 0);
    assert!(!walked.runs_off_end);
}
