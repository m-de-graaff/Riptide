//! Builders of synthetic containers for the tests.
//!
//! No shader of a real title may enter the repository, so every test input
//! is assembled here from the layouts the modules document.

use crate::inflate::zlib_stored;

/// Appends a 32-bit field.
fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a 64-bit field.
fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Builds a DXBC container from named parts.
pub(crate) fn dxbc(parts: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let table_end = 0x20 + 4 * parts.len();
    let total = table_end + parts.iter().map(|(_, data)| 8 + data.len()).sum::<usize>();
    let mut out = b"DXBC".to_vec();
    out.extend_from_slice(&[0xAB; 16]);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    put32(&mut out, total as u32);
    put32(&mut out, parts.len() as u32);
    let mut offset = table_end;
    for (_, data) in parts {
        put32(&mut out, offset as u32);
        offset += 8 + data.len();
    }
    for (name, data) in parts {
        out.extend_from_slice(*name);
        put32(&mut out, data.len() as u32);
        out.extend_from_slice(data);
    }
    out
}

/// One element of a synthetic signature.
pub(crate) struct TestElement {
    pub name: &'static str,
    pub index: u32,
    pub system_value: u32,
    pub component_type: u32,
    pub register: u32,
    pub mask: u8,
}

/// Builds the data of a signature part.
pub(crate) fn signature(elements: &[TestElement]) -> Vec<u8> {
    let names_start = 8 + 32 * elements.len();
    let mut names = Vec::new();
    let mut out = Vec::new();
    put32(&mut out, elements.len() as u32);
    put32(&mut out, 8);
    for element in elements {
        put32(&mut out, 0);
        put32(&mut out, (names_start + names.len()) as u32);
        put32(&mut out, element.index);
        put32(&mut out, element.system_value);
        put32(&mut out, element.component_type);
        put32(&mut out, element.register);
        out.extend_from_slice(&[element.mask, element.mask, 0, 0]);
        put32(&mut out, 0);
        names.extend_from_slice(element.name.as_bytes());
        names.push(0);
    }
    out.extend_from_slice(&names);
    out
}

/// Builds the data of a `PSV0` part of the newest layout this crate reads:
/// a pixel shader that binds a constant buffer and an unbounded texture
/// array, with one input (`TEXCOORD2`, row 1) and one output (a colour
/// target, row 0).
pub(crate) fn validation() -> Vec<u8> {
    let mut out = Vec::new();
    put32(&mut out, 52);
    // Stage facts of a pixel shader: no depth output, per-sample frequency.
    out.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    put32(&mut out, 4);
    put32(&mut out, 64);
    // Kind 0 (pixel), no view id, kind word, 1 input, 1 output, 0 patch
    // elements, 1 input vector, 1 output vector in stream 0.
    out.extend_from_slice(&[0, 0, 0, 0, 1, 1, 0, 1, 1, 0, 0, 0]);
    for size in [8u32, 8, 1] {
        put32(&mut out, size);
    }
    // The entry point's name is at offset 10 of the string table.
    put32(&mut out, 10);

    put32(&mut out, 2);
    put32(&mut out, 24);
    for binding in [[2u32, 0, 3, 3, 13, 0], [3, 1, 0, u32::MAX, 2, 0]] {
        for field in binding {
            put32(&mut out, field);
        }
    }

    let strings = b"\0TEXCOORD\0main\0\0";
    put32(&mut out, strings.len() as u32);
    out.extend_from_slice(strings);
    put32(&mut out, 2);
    put32(&mut out, 2);
    put32(&mut out, 0);

    put32(&mut out, 16);
    // Input: name at 1, index entry 0, 1 row at row 1, 2 columns from
    // column 1 and allocated, ordinary semantic, float, linear.
    put32(&mut out, 1);
    put32(&mut out, 0);
    out.extend_from_slice(&[1, 1, 0x52, 0, 3, 2, 0, 0]);
    // Output: unnamed, index entry 1, 1 row at row 0, 4 columns, allocated,
    // semantic kind 16 (target).
    put32(&mut out, 0);
    put32(&mut out, 1);
    out.extend_from_slice(&[1, 0, 0x44, 16, 3, 0, 0, 0]);
    out
}

/// One program of a synthetic machine code structure.
pub(crate) struct TestProgram {
    /// Where the stage's slot sits in the payload (0x30 for a pixel shader).
    pub slot: usize,
    pub wave32: bool,
    pub vector_registers: u32,
    pub scalar_registers: u32,
    pub root_parameters: Vec<[u8; 20]>,
    pub used_resources: Vec<[u8; 20]>,
    /// The stage data, which follows the record directly.
    pub stage_data: Vec<u8>,
    pub code: Vec<u32>,
}

impl TestProgram {
    /// A program with the given code and nothing else of note.
    pub(crate) fn new(slot: usize, wave32: bool, code: &[u32]) -> Self {
        TestProgram {
            slot,
            wave32,
            vector_registers: 8,
            scalar_registers: 16,
            root_parameters: Vec::new(),
            used_resources: Vec::new(),
            stage_data: Vec::new(),
            code: code.to_vec(),
        }
    }
}

/// The size of the slot table at the start of a payload.
const SLOT_TABLE_SIZE: usize = 0x68;

/// Builds an `FSSM` structure holding the given programs.
pub(crate) fn machine_code(programs: &[TestProgram]) -> Vec<u8> {
    let mut payload = Vec::new();
    put64(&mut payload, 6);
    while payload.len() < SLOT_TABLE_SIZE {
        put64(&mut payload, 0xDEAD_BEEF_DEAD_BEEF);
    }
    for program in programs {
        let record_at = payload.len();
        payload[program.slot..program.slot + 8].copy_from_slice(&(record_at as u64).to_le_bytes());
        let roots_at = record_at + 0x54 + program.stage_data.len();
        let used_at = roots_at + 20 * program.root_parameters.len();
        let code_at = used_at + 20 * program.used_resources.len();
        let mut record = vec![0u8; 0x54];
        record[0x00..0x08].copy_from_slice(&(code_at as u64).to_le_bytes());
        record[0x08..0x10].copy_from_slice(&(4 * program.code.len() as u64).to_le_bytes());
        let flags = if program.wave32 { 1u32 << 5 } else { 0 };
        record[0x14..0x18].copy_from_slice(&flags.to_le_bytes());
        record[0x28..0x30].copy_from_slice(&(roots_at as u64).to_le_bytes());
        record[0x30..0x38].copy_from_slice(&(used_at as u64).to_le_bytes());
        record[0x38..0x3C].copy_from_slice(&(program.root_parameters.len() as u32).to_le_bytes());
        record[0x3C..0x40].copy_from_slice(&(program.used_resources.len() as u32).to_le_bytes());
        record[0x4C..0x50].copy_from_slice(&512u32.to_le_bytes());
        let registers = program.vector_registers | program.scalar_registers << 20;
        record[0x50..0x54].copy_from_slice(&registers.to_le_bytes());
        payload.extend_from_slice(&record);
        payload.extend_from_slice(&program.stage_data);
        for entry in program
            .root_parameters
            .iter()
            .chain(&program.used_resources)
        {
            payload.extend_from_slice(entry);
        }
        for word in &program.code {
            put32(&mut payload, *word);
        }
    }
    let mut out = b"FSSM".to_vec();
    put32(&mut out, 0x101);
    put64(&mut out, payload.len() as u64);
    out.extend_from_slice(&payload);
    out
}

/// Wraps a machine code structure in the packed wrapper, as a zlib stream of
/// stored blocks.
pub(crate) fn packed(structure: &[u8]) -> Vec<u8> {
    let stream = zlib_stored(structure);
    let mut out = b"XBCH".to_vec();
    put32(&mut out, 3);
    put64(&mut out, stream.len() as u64);
    put64(&mut out, structure.len() as u64);
    out.extend_from_slice(&stream);
    out
}

/// Builds the data of a console `DXIL` part: the program header for a shader
/// of the given kind at shader model 6.6, the blob table, and the blobs.
pub(crate) fn dxil_part(kind: u32, blobs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let table_end = 0x24 + 12 * blobs.len();
    let total = table_end + blobs.iter().map(|(_, data)| data.len()).sum::<usize>();
    let mut out = Vec::new();
    put32(&mut out, kind << 16 | 0x66);
    put32(&mut out, (total / 4) as u32);
    out.extend_from_slice(b"DXIL");
    put32(&mut out, 0x106);
    // The bitcode field: 4 bytes long, pointing at the first blob.
    put32(&mut out, (table_end - 8) as u32);
    put32(&mut out, 4);
    out.extend_from_slice(b"XDAT");
    put32(&mut out, blobs.len() as u32);
    put32(&mut out, 0x24);
    let mut offset = table_end;
    for (kind, data) in blobs {
        put32(&mut out, *kind);
        put32(&mut out, offset as u32);
        put32(&mut out, data.len() as u32);
        offset += data.len();
    }
    for (_, data) in blobs {
        out.extend_from_slice(data);
    }
    out
}

/// A small pixel shader program: a branch over a move with a literal, the
/// end of the program, and one word of data after it.
pub(crate) const SMALL_PROGRAM: [u32; 5] = [
    0xBF88_0002, // s_cbranch_execz 2
    0xBE80_03FF, // s_mov_b32 s0, <literal>
    0x1234_5678,
    0xBF81_0000, // s_endpgm
    0xFFFF_FFFF, // data
];

/// Builds a whole container: signatures, validation, feature flags, hash,
/// and a `DXIL` part whose machine code holds [`SMALL_PROGRAM`] as a pixel
/// shader. `pack` chooses between the packed and the plain machine code blob.
pub(crate) fn pixel_container(pack: bool) -> Vec<u8> {
    let inputs = signature(&[TestElement {
        name: "TEXCOORD",
        index: 2,
        system_value: 0,
        component_type: 3,
        register: 1,
        mask: 0b0110,
    }]);
    let outputs = signature(&[TestElement {
        name: "SV_Target",
        index: 0,
        system_value: 64,
        component_type: 3,
        register: 0,
        mask: 0b1111,
    }]);
    let mut program = TestProgram::new(0x30, false, &SMALL_PROGRAM);
    // A descriptor table in user-data word 2, slot 1, and a texture read
    // through it.
    program.root_parameters.push({
        let mut entry = [0u8; 20];
        entry[0] = 3;
        entry[8] = 1;
        entry[12] = 4;
        entry[16] = 2;
        entry[17] = 1;
        entry
    });
    program.used_resources.push({
        let mut entry = [0u8; 20];
        entry[1] = 3;
        entry[4] = 8;
        entry[8] = 4;
        entry[12] = 8;
        entry[16] = 8;
        entry[17] = 2;
        entry[18] = 1;
        entry
    });
    let structure = machine_code(&[program]);
    let code_blob = if pack {
        (0x0D, packed(&structure))
    } else {
        (0x0C, structure)
    };
    let dxil = dxil_part(
        0,
        &[
            (0x07, vec![0x66, 0, 0, 0]),
            (0x0A, b"xbop\x01\x00\x00\x00".to_vec()),
            code_blob,
            (0x10, vec![0x5A; 16]),
        ],
    );
    let mut hash = vec![0, 0, 0, 0];
    hash.extend_from_slice(&[0x11; 16]);
    dxbc(&[
        (b"SFI0", 0x8000_0001u64.to_le_bytes().to_vec()),
        (b"ISG1", inputs),
        (b"OSG1", outputs),
        (b"PSV0", validation()),
        (b"HASH", hash),
        (b"DXIL", dxil),
        (b"PRIV", vec![1, 2, 3, 4]),
    ])
}
