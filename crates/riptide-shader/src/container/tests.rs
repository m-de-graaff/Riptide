//! Tests of the container readers on synthetic containers.

use super::psv::{ResourceType, StageDetails};
use super::signature::{ComponentType, SystemValue};
use super::testing::{
    SMALL_PROGRAM, TestProgram, dxbc, dxil_part, machine_code, packed, pixel_container, signature,
    validation,
};
use super::xbox::{BlobKind, HardwareStage, MachineCode, unpack};
use super::{Container, FourCc, PipelineValidation, ShaderKind, Signature, XboxDxil};
use crate::Error;
use crate::gfx10::WaveSize;

/// Reads everything this crate can read from a container. Used to check that
/// damaged input gives an error and not a panic.
fn read_everything(bytes: &[u8]) -> Result<(), Error> {
    let container = Container::parse(bytes)?;
    container.input_signature()?;
    container.output_signature()?;
    container.patch_signature()?;
    container.pipeline_validation()?;
    let _ = container.feature_flags();
    let _ = container.shader_hash();
    if let Some(dxil) = container.xbox_dxil()? {
        let _ = dxil.bitcode();
        let _ = dxil.compiler_options();
        dxil.machine_code()?;
    }
    crate::summarize(bytes)?;
    Ok(())
}

#[test]
fn splits_a_container_into_parts() {
    let bytes = dxbc(&[(b"SFI0", vec![1, 2, 3, 4]), (b"HASH", vec![9; 20])]);
    let container = Container::parse(&bytes).unwrap();
    assert_eq!(container.digest, [0xAB; 16]);
    assert_eq!(container.version, (1, 0));
    assert_eq!(container.parts.len(), 2);
    assert_eq!(container.parts[0].offset, 0x28);
    assert_eq!(
        container.part(FourCc::SFI0).unwrap().data,
        &[1, 2, 3, 4][..]
    );
    assert_eq!(container.part(FourCc::HASH).unwrap().data.len(), 20);
    assert!(container.part(FourCc::DXIL).is_none());
    assert_eq!(container.shader_hash().unwrap().digest, [9; 16]);
    // Four bytes are too few for the feature flags.
    assert_eq!(container.feature_flags(), None);
    assert_eq!(FourCc::PSV0.to_string(), "PSV0");
    assert_eq!(FourCc([0, b'a', 0xFF, b' ']).to_string(), ".a..");
}

#[test]
fn ignores_bytes_after_the_stated_size() {
    let mut bytes = dxbc(&[(b"PRIV", vec![7; 5])]);
    bytes.extend_from_slice(&[0xEE; 9]);
    let container = Container::parse(&bytes).unwrap();
    assert_eq!(container.part(FourCc::PRIV).unwrap().data, &[7; 5][..]);
}

#[test]
fn reads_the_whole_synthetic_shader() {
    for pack in [false, true] {
        let bytes = pixel_container(pack);
        let container = Container::parse(&bytes).unwrap();
        let names: Vec<String> = container.parts.iter().map(|p| p.name.to_string()).collect();
        assert_eq!(
            names,
            ["SFI0", "ISG1", "OSG1", "PSV0", "HASH", "DXIL", "PRIV"]
        );
        assert_eq!(container.feature_flags(), Some(0x8000_0001));
        assert_eq!(container.shader_hash().unwrap().digest, [0x11; 16]);

        let inputs = container.input_signature().unwrap().unwrap();
        assert_eq!(inputs.elements.len(), 1);
        let input = &inputs.elements[0];
        assert_eq!(input.semantic_name, "TEXCOORD");
        assert_eq!(input.semantic_index, 2);
        assert_eq!(input.system_value, SystemValue::NONE);
        assert_eq!(input.component_type, ComponentType::Float32);
        assert_eq!(input.register, 1);
        assert_eq!(input.mask, 0b0110);
        assert_eq!(inputs.element_at(1, 1), Some(input));
        assert_eq!(inputs.element_at(1, 0), None);
        assert_eq!(inputs.element_at(0, 1), None);
        assert_eq!(inputs.element_at(1, 200), None);

        let outputs = container.output_signature().unwrap().unwrap();
        assert_eq!(outputs.elements[0].semantic_name, "SV_Target");
        assert_eq!(outputs.elements[0].system_value, SystemValue::TARGET);
        assert_eq!(outputs.elements[0].system_value.name(), Some("Target"));
        assert_eq!(container.patch_signature().unwrap(), None);

        let dxil = container.xbox_dxil().unwrap().unwrap();
        assert_eq!(dxil.header.kind, ShaderKind::Pixel);
        assert_eq!(dxil.header.shader_model, (6, 6));
        assert_eq!(dxil.header.dxil_version, 0x106);
        assert_eq!(dxil.header.bitcode_size, 4);
        assert_eq!(dxil.bitcode(), Some(&[0x66, 0, 0, 0][..]));
        assert!(dxil.has_table);
        assert_eq!(dxil.table_word, 0x24);
        let kinds: Vec<BlobKind> = dxil.blobs.iter().map(|blob| blob.kind).collect();
        let code_kind = if pack {
            BlobKind::PackedMachineCode
        } else {
            BlobKind::MachineCode
        };
        assert_eq!(
            kinds,
            [
                BlobKind::Program,
                BlobKind::CompilerOptions,
                code_kind,
                BlobKind::ShaderHash
            ]
        );
        assert_eq!(dxil.blobs[1].magic(), Some(*b"xbop"));
        assert_eq!(dxil.compiler_options(), Some(&b"xbop\x01\x00\x00\x00"[..]));
        assert_eq!(dxil.blob(BlobKind::ShaderHash).unwrap().data, &[0x5A; 16]);
        assert_eq!(code_kind.name().contains("packed"), pack);

        let code = dxil.machine_code().unwrap();
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].packed, pack);
        assert_eq!(code[0].version, 0x101);
        assert_eq!(code[0].programs.len(), 1);
        let program = &code[0].programs[0];
        assert_eq!(program.stage, HardwareStage::Pixel);
        assert_eq!(program.stage.short_name(), "PS");
        assert_eq!(program.wave, WaveSize::Wave64);
        assert_eq!(program.vector_registers, 8);
        assert_eq!(program.scalar_registers, 16);
        assert_eq!(program.group_shared_bytes, 512);
        assert_eq!(program.code, SMALL_PROGRAM);
        assert_eq!(program.record_offset, 0x68);
        assert_eq!(program.code_offset, 0x68 + 0x54 + 40);
        assert_eq!(program.root_parameters.len(), 1);
        let root = program.root_parameters[0];
        assert_eq!(
            (
                root.root_type,
                root.register_count,
                root.base_register,
                root.user_data,
                root.slot
            ),
            (3, 1, 4, 2, 1)
        );
        assert_eq!(program.used_resources.len(), 1);
        let used = program.used_resources[0];
        assert_eq!(
            (
                used.resource_type,
                used.root_type,
                used.table_offset_words,
                used.lower_bound,
                used.stride,
                used.descriptor_words,
                used.user_data,
                used.slot
            ),
            (0, 3, 8, 4, 8, 8, 2, 1)
        );
        assert_eq!(read_everything(&bytes), Ok(()));
    }
}

#[test]
fn reads_pipeline_validation() {
    let validation = PipelineValidation::parse(&validation()).unwrap();
    assert_eq!(validation.info_size, 52);
    assert_eq!(validation.kind, Some(ShaderKind::Pixel));
    assert_eq!(
        validation.stage_details(),
        StageDetails::Pixel {
            depth_output: false,
            sample_frequency: true
        }
    );
    assert_eq!(
        (validation.min_wave_lanes, validation.max_wave_lanes),
        (4, 64)
    );
    assert!(!validation.uses_view_id);
    assert_eq!(validation.input_vectors, 1);
    assert_eq!(validation.output_vectors, [1, 0, 0, 0]);
    assert_eq!(validation.thread_group_size, Some([8, 8, 1]));
    assert_eq!(validation.entry_name.as_deref(), Some("main"));

    assert_eq!(validation.resources.len(), 2);
    let buffer = validation.resources[0];
    assert_eq!(buffer.resource_type, ResourceType::ConstantBuffer);
    assert_eq!(
        (buffer.space, buffer.lower_bound, buffer.upper_bound),
        (0, 3, 3)
    );
    assert_eq!(buffer.resource_kind, Some(13));
    let textures = validation.resources[1];
    assert_eq!(textures.resource_type, ResourceType::SrvTyped);
    assert_eq!(
        (textures.space, textures.lower_bound, textures.upper_bound),
        (1, 0, u32::MAX)
    );

    assert_eq!(validation.inputs.len(), 1);
    let input = &validation.inputs[0];
    assert_eq!(input.semantic_name, "TEXCOORD");
    assert_eq!(input.semantic_indexes, [2]);
    assert_eq!((input.rows, input.start_row), (1, 1));
    assert_eq!((input.columns, input.start_column), (2, 1));
    assert!(input.allocated);
    assert_eq!(validation.outputs.len(), 1);
    let output = &validation.outputs[0];
    assert_eq!(output.semantic_name, "");
    assert_eq!(output.semantic_indexes, [0]);
    assert_eq!(output.semantic_kind, 16);
    assert_eq!(output.columns, 4);
    assert!(validation.patch_or_primitive.is_empty());
    assert_eq!(
        validation.dependency_tables_offset,
        super::testing::validation().len()
    );
}

#[test]
fn reads_the_oldest_validation_layout() {
    // 24 bytes of runtime info and no resources: nothing follows.
    let mut part = 24u32.to_le_bytes().to_vec();
    part.extend_from_slice(&[0; 24]);
    part.extend_from_slice(&0u32.to_le_bytes());
    let validation = PipelineValidation::parse(&part).unwrap();
    assert_eq!(validation.kind, None);
    assert_eq!(validation.stage_details(), StageDetails::None);
    assert_eq!(validation.thread_group_size, None);
    assert!(validation.resources.is_empty());
}

#[test]
fn reads_several_stages_and_both_wave_sizes() {
    let structure = machine_code(&[
        TestProgram::new(0x28, true, &[0xBF81_0000]),
        TestProgram::new(0x60, false, &[0xBF80_0000, 0xBF81_0000]),
    ]);
    let code = MachineCode::parse(structure, false).unwrap();
    let stages: Vec<(HardwareStage, WaveSize, usize)> = code
        .programs
        .iter()
        .map(|program| (program.stage, program.wave, program.code.len()))
        .collect();
    assert_eq!(
        stages,
        [
            (HardwareStage::Vertex, WaveSize::Wave32, 1),
            (HardwareStage::Compute, WaveSize::Wave64, 2)
        ]
    );
}

#[test]
fn a_pc_layout_has_no_table_and_no_machine_code() {
    let mut part = Vec::new();
    part.extend_from_slice(&(5u32 << 16 | 0x60).to_le_bytes());
    part.extend_from_slice(&10u32.to_le_bytes());
    part.extend_from_slice(b"DXIL");
    part.extend_from_slice(&0x100u32.to_le_bytes());
    part.extend_from_slice(&16u32.to_le_bytes());
    part.extend_from_slice(&16u32.to_le_bytes());
    part.extend_from_slice(&[0x42, 0x43, 0xC0, 0xDE]);
    part.extend_from_slice(&[0; 12]);
    let dxil = XboxDxil::parse(&part).unwrap();
    assert_eq!(dxil.header.kind, ShaderKind::Compute);
    assert!(!dxil.has_table);
    assert_eq!(dxil.bitcode().unwrap().len(), 16);
    assert!(dxil.machine_code().unwrap().is_empty());
    let bytes = dxbc(&[(b"DXIL", part)]);
    assert_eq!(crate::summarize(&bytes), Err(Error::NoMachineCode));
    let bytes = dxbc(&[(b"PRIV", vec![0; 4])]);
    assert_eq!(crate::summarize(&bytes), Err(Error::NoMachineCode));
}

#[test]
fn truncation_at_every_length_is_an_error() {
    for pack in [false, true] {
        let bytes = pixel_container(pack);
        for length in 0..bytes.len() {
            assert!(
                read_everything(&bytes[..length]).is_err(),
                "length {length}"
            );
        }
    }
    // The same for each structure on its own, where the enclosing size
    // fields do not catch the cut first.
    let part = signature(&[super::testing::TestElement {
        name: "POSITION",
        index: 0,
        system_value: 1,
        component_type: 3,
        register: 0,
        mask: 0xF,
    }]);
    for length in 0..part.len() {
        assert!(
            Signature::parse(&part[..length]).is_err(),
            "length {length}"
        );
    }
    let part = validation();
    for length in 0..part.len() {
        assert!(
            PipelineValidation::parse(&part[..length]).is_err(),
            "length {length}"
        );
    }
    let structure = machine_code(&[TestProgram::new(0x30, false, &SMALL_PROGRAM)]);
    for length in 0..structure.len() {
        assert!(
            MachineCode::parse(structure[..length].to_vec(), false).is_err(),
            "length {length}"
        );
    }
    let wrapped = packed(&structure);
    for length in 0..wrapped.len() {
        assert!(unpack(&wrapped[..length]).is_err(), "length {length}");
    }
    let part = dxil_part(0, &[(0x0C, structure)]);
    for length in 0..part.len() {
        let read = XboxDxil::parse(&part[..length]).and_then(|dxil| dxil.machine_code());
        if (0x18..0x1C).contains(&length) {
            // Cut inside the table's magic, the part is a whole header of
            // the PC layout, which holds no machine code.
            assert_eq!(read, Ok(Vec::new()), "length {length}");
        } else {
            assert!(read.is_err(), "length {length}");
        }
    }
}

#[test]
fn any_single_damaged_byte_is_survived() {
    // Damage does not always make a container unreadable, but reading must
    // never panic, whatever byte is changed to whatever value.
    for pack in [false, true] {
        let bytes = pixel_container(pack);
        for at in 0..bytes.len() {
            for value in [0x00, 0x01, 0x7F, 0x80, 0xFF] {
                let mut damaged = bytes.clone();
                damaged[at] = value;
                let _ = read_everything(&damaged);
            }
        }
    }
}

/// Overwrites a 32-bit field.
fn poke32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Overwrites a 64-bit field.
fn poke64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn container_offsets_outside_the_data_are_errors() {
    assert_eq!(Container::parse(b"nope"), Err(Error::NotDxbc));
    assert_eq!(Container::parse(b""), Err(Error::NotDxbc));
    let good = dxbc(&[(b"SFI0", vec![0; 8])]);

    let mut bytes = good.clone();
    poke32(&mut bytes, 0x18, 0x10);
    assert!(matches!(Container::parse(&bytes), Err(Error::Malformed(_))));
    let mut bytes = good.clone();
    poke32(&mut bytes, 0x18, good.len() as u32 + 1);
    assert!(matches!(Container::parse(&bytes), Err(Error::Truncated(_))));
    let mut bytes = good.clone();
    poke32(&mut bytes, 0x1C, u32::MAX);
    assert!(matches!(Container::parse(&bytes), Err(Error::Malformed(_))));
    for offset in [good.len() as u32 - 4, good.len() as u32, u32::MAX] {
        let mut bytes = good.clone();
        poke32(&mut bytes, 0x20, offset);
        assert!(matches!(
            Container::parse(&bytes),
            Err(Error::OutOfBounds(_))
        ));
    }
    let mut bytes = good.clone();
    poke32(&mut bytes, 0x28, 9);
    assert!(matches!(
        Container::parse(&bytes),
        Err(Error::OutOfBounds(_))
    ));
    let mut bytes = good;
    poke32(&mut bytes, 0x28, u32::MAX);
    assert!(matches!(
        Container::parse(&bytes),
        Err(Error::OutOfBounds(_))
    ));
}

#[test]
fn signature_offsets_outside_the_data_are_errors() {
    let good = signature(&[super::testing::TestElement {
        name: "NORMAL",
        index: 0,
        system_value: 0,
        component_type: 3,
        register: 2,
        mask: 7,
    }]);
    assert!(Signature::parse(&good).is_ok());
    // The element count, the element offset and the name offset.
    for (at, value) in [(0, 2), (0, u32::MAX), (4, 16), (4, u32::MAX), (12, 4096)] {
        let mut part = good.clone();
        poke32(&mut part, at, value);
        assert!(Signature::parse(&part).is_err(), "field at {at} = {value}");
    }
    // A name with no terminator.
    let mut part = good.clone();
    let last = part.len() - 1;
    part[last] = b'X';
    assert_eq!(
        Signature::parse(&part),
        Err(Error::Truncated("a semantic name"))
    );
}

#[test]
fn validation_sizes_outside_the_data_are_errors() {
    let good = validation();
    // The runtime info size, the binding count and size, the string table
    // size, the index table size, the element size, an element's index
    // offset.
    let string_table = 4 + 52 + 8 + 48;
    let index_table = string_table + 4 + 16;
    let element_size = index_table + 4 + 8;
    for (at, value) in [
        (0, 8),
        (0, u32::MAX),
        (56, 1000),
        (60, 8),
        (60, u32::MAX),
        (string_table, 4000),
        (index_table, u32::MAX),
        (element_size, 4),
        (element_size, 4000),
        (element_size + 4 + 4, 2),
    ] {
        let mut part = good.clone();
        poke32(&mut part, at, value);
        assert!(
            PipelineValidation::parse(&part).is_err(),
            "field at {at} = {value}"
        );
    }
}

#[test]
fn dxil_part_errors() {
    let structure = machine_code(&[TestProgram::new(0x30, false, &SMALL_PROGRAM)]);
    let good = dxil_part(0, &[(0x0C, structure.clone())]);
    assert!(XboxDxil::parse(&good).is_ok());

    let mut part = good.clone();
    part[8] = b'X';
    assert!(matches!(XboxDxil::parse(&part), Err(Error::Malformed(_))));
    // The blob count, a blob's offset and a blob's size.
    for (at, value) in [
        (0x1C, 1000),
        (0x1C, u32::MAX),
        (0x28, good.len() as u32),
        (0x28, u32::MAX),
        (0x2C, structure.len() as u32 + 1),
        (0x2C, u32::MAX),
    ] {
        let mut part = good.clone();
        poke32(&mut part, at, value);
        assert!(
            matches!(XboxDxil::parse(&part), Err(Error::OutOfBounds(_))),
            "field at {at:#x} = {value}"
        );
    }
    // A bitcode field that points outside is not an error, because the
    // console's field is not meaningful; it just yields nothing.
    let mut part = good;
    poke32(&mut part, 0x10, u32::MAX);
    assert_eq!(XboxDxil::parse(&part).unwrap().bitcode(), None);
}

#[test]
fn machine_code_errors() {
    let good = machine_code(&[TestProgram::new(0x30, false, &SMALL_PROGRAM)]);
    let parse = |bytes: &[u8]| MachineCode::parse(bytes.to_vec(), false);
    assert!(parse(&good).is_ok());
    let payload = 0x10;
    let record = payload + 0x68;

    let mut bytes = good.clone();
    bytes[0] = b'X';
    assert!(matches!(parse(&bytes), Err(Error::Malformed(_))));
    let mut bytes = good.clone();
    poke32(&mut bytes, 4, 0x102);
    assert_eq!(
        parse(&bytes),
        Err(Error::UnknownVersion {
            what: "the machine code structure",
            version: 0x102
        })
    );
    let mut bytes = good.clone();
    poke64(&mut bytes, 8, good.len() as u64);
    assert!(matches!(parse(&bytes), Err(Error::Malformed(_))));
    let mut bytes = good.clone();
    poke64(&mut bytes, payload, 7);
    assert!(matches!(parse(&bytes), Err(Error::Malformed(_))));

    // The record offset, the code offset, the code size, and the two tables
    // once they are given a count.
    for (at, value) in [
        (payload + 0x30, good.len() as u64),
        (payload + 0x30, u64::MAX),
        (payload + 0x30, u64::MAX - 0x20),
        (record, good.len() as u64),
        (record, u64::MAX),
        (record + 8, 4096),
        (record + 8, u64::MAX),
    ] {
        let mut bytes = good.clone();
        poke64(&mut bytes, at, value);
        assert!(
            matches!(parse(&bytes), Err(Error::OutOfBounds(_))),
            "field at {at:#x} = {value:#x}"
        );
    }
    for count_at in [record + 0x38, record + 0x3C] {
        let mut bytes = good.clone();
        poke32(&mut bytes, count_at, 1_000_000);
        assert!(matches!(parse(&bytes), Err(Error::OutOfBounds(_))));
        let mut bytes = good.clone();
        poke32(&mut bytes, count_at, u32::MAX);
        assert!(matches!(parse(&bytes), Err(Error::OutOfBounds(_))));
    }
    // Code that is not a whole number of words.
    let mut bytes = good;
    poke64(&mut bytes, record + 8, 6);
    assert!(matches!(parse(&bytes), Err(Error::Malformed(_))));
}

#[test]
fn packed_wrapper_errors() {
    let structure = machine_code(&[TestProgram::new(0x30, false, &SMALL_PROGRAM)]);
    let good = packed(&structure);
    assert_eq!(unpack(&good).unwrap(), structure);

    let mut bytes = good.clone();
    bytes[3] = b'X';
    assert!(matches!(unpack(&bytes), Err(Error::Malformed(_))));
    let mut bytes = good.clone();
    poke32(&mut bytes, 4, 2);
    assert!(matches!(unpack(&bytes), Err(Error::UnknownVersion { .. })));
    let mut bytes = good.clone();
    poke64(&mut bytes, 8, good.len() as u64);
    assert!(matches!(unpack(&bytes), Err(Error::OutOfBounds(_))));
    // An unpacked size smaller than the stream's output, larger than it,
    // and absurd.
    let mut bytes = good.clone();
    poke64(&mut bytes, 16, structure.len() as u64 - 1);
    assert_eq!(
        unpack(&bytes),
        Err(Error::Inflate(crate::inflate::InflateError::TooLarge))
    );
    let mut bytes = good.clone();
    poke64(&mut bytes, 16, structure.len() as u64 + 1);
    assert!(matches!(unpack(&bytes), Err(Error::Malformed(_))));
    let mut bytes = good.clone();
    poke64(&mut bytes, 16, u64::MAX);
    assert!(matches!(unpack(&bytes), Err(Error::Malformed(_))));
    // A damaged stream.
    let mut bytes = good;
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    assert_eq!(
        unpack(&bytes),
        Err(Error::Inflate(crate::inflate::InflateError::BadChecksum))
    );
}

#[test]
fn errors_print_as_sentences() {
    assert_eq!(Error::NotDxbc.to_string(), "not a DXBC container");
    assert_eq!(
        Error::OutOfBounds("a blob").to_string(),
        "a blob lies outside the data"
    );
    assert_eq!(
        Error::UnknownVersion {
            what: "the machine code structure",
            version: 0x102
        }
        .to_string(),
        "the machine code structure has the unknown version 0x102"
    );
}
