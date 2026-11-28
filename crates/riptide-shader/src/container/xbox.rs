//! The console's `DXIL` part: the program header, the table of blobs after
//! it, and the machine code inside one of those blobs.
//!
//! On a PC the `DXIL` part is a short program header followed by LLVM
//! bitcode. The console compiler keeps the header and adds a table, `XDAT`,
//! that indexes blobs stored in the rest of the part. The layouts below were
//! worked out from shipped titles by other projects; nothing here comes from
//! published documentation, and the doc comments say where a field's meaning
//! is a guess.
//!
//! ```text
//! 0x00  program version, u32: kind << 16 | shader model major << 4 | minor
//! 0x04  size of the program in 32-bit words, u32
//! 0x08  "DXIL"
//! 0x0C  DXIL version, u32
//! 0x10  offset of the bitcode from 0x08, u32
//! 0x14  size of the bitcode in bytes, u32
//! 0x18  "XDAT"                       (absent on a PC, where bitcode starts here)
//! 0x1C  number of table entries, u32
//! 0x20  u32, seen as 0x24: taken to be the offset of the first entry
//! 0x24  entries of 12 bytes: kind u32, offset from the start of the part u32,
//!       size u32
//! ```
//!
//! The kinds of blob seen ([`BlobKind`]): the program header with its bitcode
//! (or a stub of a few bytes when the bitcode was stripped), the compiler
//! options, the machine code stored plainly or packed, the shader hash, and
//! two whose contents are not known.
//!
//! Packed machine code is wrapped so:
//!
//! ```text
//! 0x00  "XBCH"
//! 0x04  version, u32 (3)
//! 0x08  size of the zlib stream, u64
//! 0x10  size once unpacked, u64
//! 0x18  the zlib stream
//! ```
//!
//! Unpacked, or stored plainly, the machine code is one `FSSM` structure:
//!
//! ```text
//! 0x00  "FSSM"
//! 0x04  version, u32 (0x101)
//! 0x08  size of the payload that follows, u64
//! payload (every offset below is from the start of the payload):
//!   0x00  number of graphics stage slots, u64 (6)
//!   0x08  offset of each hardware stage's record, u64 each: LS, HS, ES, GS,
//!         VS, PS at 0x08 to 0x30, mesh at 0x48, compute at 0x60. A stage
//!         that is not present holds 0xDEADBEEF_DEADBEEF.
//! record, 0x54 bytes:
//!   0x00  offset of the code, u64
//!   0x08  size of the code in bytes, u64
//!   0x14  flags, u32: bit 5 set means 32 lanes per wave, clear means 64
//!   0x28  offset of the root parameter table, u64
//!   0x30  offset of the used-resource table, u64
//!   0x38  number of root parameters, u32
//!   0x3C  number of used resources, u32
//!   0x4C  bytes of group-shared memory, u32
//!   0x50  u32: bits 0-9 vector registers used, bits 20-27 scalar registers used
//! root parameter, 20 bytes:
//!   u32 root type, u32 offset in its table (32-bit words), u32 count of
//!   scalar registers, u32 first shader register, u8 first user-data word,
//!   u8 root signature slot, u16 zero
//! used resource, 20 bytes:
//!   u8 resource type, u8 root type, u16 zero, u32 offset in its table
//!   (32-bit words), u32 first shader register, u32 stride, u8 size of the
//!   descriptor in 32-bit words, u8 first user-data word, u8 root signature
//!   slot, u8 zero
//! ```
//!
//! After each record comes data particular to the stage: the pixel shader's
//! interpolated inputs and the code patches for its colour outputs, the
//! vertex shader's attributes and outputs, the compute shader's thread group
//! size. [`super::stage`] reads it, from [`MachineCode::payload`] at
//! [`Program::record_offset`].
//!
//! A structure whose version is not the one listed is rejected with
//! [`Error::UnknownVersion`], so that a change of format shows up as an error
//! and not as wrong code.

use super::ShaderKind;
use crate::Error;
use crate::bytes::{span, u8_at, u32_at, u64_at};
use crate::gfx10::WaveSize;
use crate::inflate;

/// Where the blob table starts in the part.
const TABLE_OFFSET: usize = 0x18;
/// The size of the blob table's header.
const TABLE_HEADER_SIZE: usize = 12;
/// The size of one entry of the blob table.
const TABLE_ENTRY_SIZE: usize = 12;
/// The version of the packed wrapper this module reads.
const PACKED_VERSION: u32 = 3;
/// The size of the packed wrapper's header.
const PACKED_HEADER_SIZE: u64 = 0x18;
/// The largest unpacked size accepted. Real machine code is a few kilobytes;
/// the limit keeps a damaged size field from asking for gigabytes.
const UNPACKED_LIMIT: u64 = 16 << 20;
/// The version of the machine code structure this module reads.
const CODE_VERSION: u32 = 0x101;
/// The size of the machine code structure's header.
const CODE_HEADER_SIZE: usize = 0x10;
/// The slot count every machine code structure seen so far states.
const GRAPHICS_SLOTS: u64 = 6;
/// The offset a slot holds when its stage is not present.
const NO_STAGE: u64 = 0xDEAD_BEEF_DEAD_BEEF;
/// The size of a stage record.
const RECORD_SIZE: u64 = 0x54;
/// The size of an entry of the root parameter and used-resource tables.
const BINDING_ENTRY_SIZE: usize = 20;
/// The flag of a record that says the program runs 32 lanes per wave.
const FLAG_WAVE32: u32 = 1 << 5;

/// The fixed header at the start of a `DXIL` part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramHeader {
    /// The kind of shader.
    pub kind: ShaderKind,
    /// The shader model, major then minor.
    pub shader_model: (u8, u8),
    /// The size of the program as the header states it, in 32-bit words.
    pub size_words: u32,
    /// The DXIL version.
    pub dxil_version: u32,
    /// The offset of the bitcode, counted from byte 8 of the part.
    pub bitcode_offset: u32,
    /// The size of the bitcode in bytes. On the console this is a few bytes:
    /// the bitcode is not there.
    pub bitcode_size: u32,
}

/// What a blob of the table holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobKind {
    /// Kind 0x07: the program header and its bitcode, or a stub where the
    /// bitcode was stripped.
    Program,
    /// Kind 0x0A: the compiler options (the blob starts with `xbop`). Its
    /// contents are not decoded.
    CompilerOptions,
    /// Kind 0x0C: machine code stored plainly, an `FSSM` structure.
    MachineCode,
    /// Kind 0x0D: machine code packed with zlib, an `XBCH` wrapper.
    PackedMachineCode,
    /// Kind 0x0E: a table whose contents are not known.
    UnknownTable,
    /// Kind 0x10: a 16-byte blob taken to be the shader hash.
    ShaderHash,
    /// Kind 0x12: a 16-byte blob whose meaning is not known.
    Unknown16,
    /// Any other kind number.
    Other(u32),
}

impl BlobKind {
    /// Converts the number the table stores.
    pub fn from_number(number: u32) -> Self {
        match number {
            0x07 => BlobKind::Program,
            0x0A => BlobKind::CompilerOptions,
            0x0C => BlobKind::MachineCode,
            0x0D => BlobKind::PackedMachineCode,
            0x0E => BlobKind::UnknownTable,
            0x10 => BlobKind::ShaderHash,
            0x12 => BlobKind::Unknown16,
            other => BlobKind::Other(other),
        }
    }

    /// A short name for listings.
    pub fn name(self) -> &'static str {
        match self {
            BlobKind::Program => "program",
            BlobKind::CompilerOptions => "compiler-options",
            BlobKind::MachineCode => "machine-code",
            BlobKind::PackedMachineCode => "packed-machine-code",
            BlobKind::UnknownTable => "unknown-table",
            BlobKind::ShaderHash => "shader-hash",
            BlobKind::Unknown16 => "unknown-16",
            BlobKind::Other(_) => "other",
        }
    }
}

/// One blob the table indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blob<'a> {
    /// What the blob holds.
    pub kind: BlobKind,
    /// The kind number as stored.
    pub kind_number: u32,
    /// Where the blob starts in the part.
    pub offset: u32,
    /// The blob's bytes.
    pub data: &'a [u8],
}

impl Blob<'_> {
    /// Returns the four bytes the blob starts with, which for most kinds are
    /// a magic such as `xbop`, `XBCH` or `FSSM`.
    pub fn magic(&self) -> Option<[u8; 4]> {
        self.data.get(..4).map(|m| [m[0], m[1], m[2], m[3]])
    }
}

/// A `DXIL` part: its header and, on the console, its blobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XboxDxil<'a> {
    /// The fixed program header.
    pub header: ProgramHeader,
    /// Whether the part has the console's blob table. Without it the part has
    /// the PC layout and holds no machine code.
    pub has_table: bool,
    /// The third word of the table header. It has only been seen as 0x24,
    /// which is where the first entry starts; that reading is a guess.
    pub table_word: u32,
    /// The blobs in table order. Empty for the PC layout.
    pub blobs: Vec<Blob<'a>>,
    part: &'a [u8],
}

impl<'a> XboxDxil<'a> {
    /// Reads the data of a `DXIL` part.
    pub fn parse(part: &'a [u8]) -> Result<Self, Error> {
        let what = "the program header";
        let version = u32_at(part, 0, what)?;
        let size_words = u32_at(part, 4, what)?;
        if span(part, 8, 4, what)? != b"DXIL" {
            return Err(Error::Malformed("the program header lacks the DXIL magic"));
        }
        let header = ProgramHeader {
            kind: ShaderKind::from_number((version >> 16) as u16),
            shader_model: ((version >> 4 & 0xF) as u8, (version & 0xF) as u8),
            size_words,
            dxil_version: u32_at(part, 12, what)?,
            bitcode_offset: u32_at(part, 16, what)?,
            bitcode_size: u32_at(part, 20, what)?,
        };
        let mut out = XboxDxil {
            header,
            has_table: false,
            table_word: 0,
            blobs: Vec::new(),
            part,
        };
        if part.get(TABLE_OFFSET..TABLE_OFFSET + 4) != Some(b"XDAT") {
            return Ok(out);
        }
        out.has_table = true;
        let count = u32_at(part, TABLE_OFFSET + 4, "the blob table header")? as usize;
        out.table_word = u32_at(part, TABLE_OFFSET + 8, "the blob table header")?;
        let entries = span(
            part,
            (TABLE_OFFSET + TABLE_HEADER_SIZE) as u64,
            count as u64 * TABLE_ENTRY_SIZE as u64,
            "the blob table",
        )?;
        for raw in entries.chunks_exact(TABLE_ENTRY_SIZE) {
            let kind_number = u32_at(raw, 0, "a blob table entry")?;
            let offset = u32_at(raw, 4, "a blob table entry")?;
            let size = u32_at(raw, 8, "a blob table entry")?;
            out.blobs.push(Blob {
                kind: BlobKind::from_number(kind_number),
                kind_number,
                offset,
                data: span(part, u64::from(offset), u64::from(size), "a blob")?,
            });
        }
        Ok(out)
    }

    /// Returns the bytes the header calls the bitcode, if its offset and size
    /// lie inside the part. On the console these are a few bytes that are not
    /// bitcode.
    pub fn bitcode(&self) -> Option<&'a [u8]> {
        let start = 8 + u64::from(self.header.bitcode_offset);
        span(
            self.part,
            start,
            u64::from(self.header.bitcode_size),
            "the bitcode",
        )
        .ok()
    }

    /// Returns the first blob of a kind.
    pub fn blob(&self, kind: BlobKind) -> Option<&Blob<'a>> {
        self.blobs.iter().find(|blob| blob.kind == kind)
    }

    /// Returns the bytes of the compiler options blob, if there is one. The
    /// contents are not decoded.
    pub fn compiler_options(&self) -> Option<&'a [u8]> {
        self.blob(BlobKind::CompilerOptions).map(|blob| blob.data)
    }

    /// Reads every machine code blob of the part, unpacking the packed ones.
    /// A part usually has one. A vertex shader can have two: the program
    /// compiled for use with and without a geometry shader. The result is
    /// empty for the PC layout.
    pub fn machine_code(&self) -> Result<Vec<MachineCode>, Error> {
        let mut out = Vec::new();
        for blob in &self.blobs {
            match blob.kind {
                BlobKind::MachineCode => out.push(MachineCode::parse(blob.data.to_vec(), false)?),
                BlobKind::PackedMachineCode => {
                    out.push(MachineCode::parse(unpack(blob.data)?, true)?);
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Unpacks an `XBCH` wrapper into the `FSSM` structure it holds.
pub fn unpack(blob: &[u8]) -> Result<Vec<u8>, Error> {
    let what = "the packed machine code header";
    if span(blob, 0, 4, what)? != b"XBCH" {
        return Err(Error::Malformed(
            "the packed machine code lacks the XBCH magic",
        ));
    }
    let version = u32_at(blob, 4, what)?;
    if version != PACKED_VERSION {
        return Err(Error::UnknownVersion {
            what: "the packed machine code wrapper",
            version,
        });
    }
    let packed_size = u64_at(blob, 8, what)?;
    let unpacked_size = u64_at(blob, 16, what)?;
    if unpacked_size > UNPACKED_LIMIT {
        return Err(Error::Malformed(
            "the unpacked size of the machine code is implausibly large",
        ));
    }
    let stream = span(
        blob,
        PACKED_HEADER_SIZE,
        packed_size,
        "the packed machine code",
    )?;
    let out = inflate::zlib(stream, unpacked_size as usize)?;
    if out.len() as u64 != unpacked_size {
        return Err(Error::Malformed(
            "the machine code unpacks to a size other than the one stated",
        ));
    }
    Ok(out)
}

/// The hardware stage a program runs as. The console GPU has more stages
/// than Direct3D has kinds of shader: with tessellation or a geometry shader
/// a vertex shader runs as the local or export stage instead of the vertex
/// stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HardwareStage {
    /// LS: a vertex shader that feeds a hull shader.
    Local,
    /// HS: a hull shader.
    Hull,
    /// ES: a vertex or domain shader that feeds a geometry shader.
    Export,
    /// GS: a geometry shader.
    Geometry,
    /// VS: the last stage before rasterisation.
    Vertex,
    /// PS: a pixel shader.
    Pixel,
    /// A mesh shader.
    Mesh,
    /// CS: a compute shader.
    Compute,
}

impl HardwareStage {
    /// The stage's conventional two-letter name.
    pub fn short_name(self) -> &'static str {
        match self {
            HardwareStage::Local => "LS",
            HardwareStage::Hull => "HS",
            HardwareStage::Export => "ES",
            HardwareStage::Geometry => "GS",
            HardwareStage::Vertex => "VS",
            HardwareStage::Pixel => "PS",
            HardwareStage::Mesh => "MS",
            HardwareStage::Compute => "CS",
        }
    }
}

/// Where each stage's record offset sits in the payload.
const STAGE_SLOTS: [(HardwareStage, usize); 8] = [
    (HardwareStage::Local, 0x08),
    (HardwareStage::Hull, 0x10),
    (HardwareStage::Export, 0x18),
    (HardwareStage::Geometry, 0x20),
    (HardwareStage::Vertex, 0x28),
    (HardwareStage::Pixel, 0x30),
    (HardwareStage::Mesh, 0x48),
    (HardwareStage::Compute, 0x60),
];

/// One root signature parameter, or one range of a descriptor table, and the
/// scalar registers the program receives it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootParameter {
    /// The root type number. Seen: 0 root constants, 1 a root descriptor
    /// (a 64-bit buffer address), 3 a descriptor table (a 32-bit heap
    /// address), 4 a sampler fixed by the root signature, 5 the resource
    /// heap, 6 the sampler heap.
    pub root_type: u32,
    /// The offset of the range within its table, in 32-bit words.
    pub table_offset_words: u32,
    /// How many scalar registers the parameter occupies.
    pub register_count: u32,
    /// The first shader register (the number of `b`, `t`, `u` or `s`).
    pub base_register: u32,
    /// The first user-data word. For the vertex, pixel and compute stages
    /// user-data word n arrives in scalar register n. The reference this was
    /// taken from says the other stages have 8 system registers first.
    pub user_data: u8,
    /// The index of the parameter in the root signature.
    pub slot: u8,
}

/// One range of descriptors the program reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsedResource {
    /// The resource type number. Seen: 0 a shader resource view, 1 a
    /// sampler, 2 a constant buffer view, 3 an unordered access view.
    pub resource_type: u8,
    /// The root type number, as in [`RootParameter::root_type`].
    pub root_type: u8,
    /// The offset of the range within its table, in 32-bit words.
    pub table_offset_words: u32,
    /// The shader register of the first descriptor read.
    pub lower_bound: u32,
    /// The stride between descriptors, as stored.
    pub stride: u32,
    /// The size of one descriptor in 32-bit words (4 or 8).
    pub descriptor_words: u8,
    /// The first user-data word, as in [`RootParameter::user_data`].
    pub user_data: u8,
    /// The index of the parameter in the root signature.
    pub slot: u8,
}

/// The program of one hardware stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// The stage the program runs as.
    pub stage: HardwareStage,
    /// How many lanes one wave of the program has. It decides the width of
    /// lane masks, and so how some instructions are read.
    pub wave: WaveSize,
    /// The record's flags as stored.
    pub flags: u32,
    /// How many vector registers the program uses.
    pub vector_registers: u16,
    /// How many scalar registers the program uses.
    pub scalar_registers: u8,
    /// Bytes of group-shared memory, as the record states them.
    pub group_shared_bytes: u32,
    /// Where the stage's record starts in the payload. The stage data, which
    /// [`MachineCode::stage_data`] reads, follows the record.
    pub record_offset: u64,
    /// Where the code starts in the payload.
    pub code_offset: u64,
    /// The root signature parameters and where they arrive.
    pub root_parameters: Vec<RootParameter>,
    /// The descriptors the program reads.
    pub used_resources: Vec<UsedResource>,
    /// The machine code as 32-bit words. Compilers place data after the end
    /// of the program, so not every word is an instruction.
    pub code: Vec<u32>,
}

/// One machine code structure (`FSSM`) and the programs in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineCode {
    /// Whether the structure was stored packed.
    pub packed: bool,
    /// The structure's version.
    pub version: u32,
    /// The payload: everything after the 16-byte header. Offsets in records
    /// count from its start.
    pub payload: Vec<u8>,
    /// The program of each stage present, in slot order.
    pub programs: Vec<Program>,
}

impl MachineCode {
    /// Reads an unpacked `FSSM` structure. `packed` only records how the
    /// structure was stored.
    pub fn parse(structure: Vec<u8>, packed: bool) -> Result<Self, Error> {
        let what = "the machine code header";
        if span(&structure, 0, 4, what)? != b"FSSM" {
            return Err(Error::Malformed("the machine code lacks the FSSM magic"));
        }
        let version = u32_at(&structure, 4, what)?;
        if version != CODE_VERSION {
            return Err(Error::UnknownVersion {
                what: "the machine code structure",
                version,
            });
        }
        let payload_size = u64_at(&structure, 8, what)?;
        let payload = &structure[CODE_HEADER_SIZE..];
        if payload_size != payload.len() as u64 {
            return Err(Error::Malformed(
                "the machine code payload size disagrees with the data",
            ));
        }
        let slots = u64_at(payload, 0, "the stage slot table")?;
        if slots != GRAPHICS_SLOTS {
            return Err(Error::Malformed(
                "the machine code states an unexpected number of stage slots",
            ));
        }
        let mut programs = Vec::new();
        for (stage, slot) in STAGE_SLOTS {
            let record_offset = u64_at(payload, slot, "the stage slot table")?;
            if record_offset != NO_STAGE {
                programs.push(program(payload, stage, record_offset)?);
            }
        }
        let payload = structure[CODE_HEADER_SIZE..].to_vec();
        Ok(MachineCode {
            packed,
            version,
            payload,
            programs,
        })
    }
}

/// Reads one stage's record, its two tables and its code.
fn program(payload: &[u8], stage: HardwareStage, record_offset: u64) -> Result<Program, Error> {
    let what = "a stage record";
    let record = span(payload, record_offset, RECORD_SIZE, what)?;
    let code_offset = u64_at(record, 0x00, what)?;
    let code_size = u64_at(record, 0x08, what)?;
    let code_bytes = span(payload, code_offset, code_size, "the machine code")?;
    if !code_bytes.len().is_multiple_of(4) {
        return Err(Error::Malformed(
            "the machine code is not a whole number of 32-bit words",
        ));
    }
    let code = code_bytes
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    let mut root_parameters = Vec::new();
    for raw in table(payload, record, 0x28, 0x38, "the root parameter table")?
        .chunks_exact(BINDING_ENTRY_SIZE)
    {
        let what = "a root parameter";
        root_parameters.push(RootParameter {
            root_type: u32_at(raw, 0, what)?,
            table_offset_words: u32_at(raw, 4, what)?,
            register_count: u32_at(raw, 8, what)?,
            base_register: u32_at(raw, 12, what)?,
            user_data: u8_at(raw, 16, what)?,
            slot: u8_at(raw, 17, what)?,
        });
    }
    let mut used_resources = Vec::new();
    for raw in table(payload, record, 0x30, 0x3C, "the used-resource table")?
        .chunks_exact(BINDING_ENTRY_SIZE)
    {
        let what = "a used resource";
        used_resources.push(UsedResource {
            resource_type: u8_at(raw, 0, what)?,
            root_type: u8_at(raw, 1, what)?,
            table_offset_words: u32_at(raw, 4, what)?,
            lower_bound: u32_at(raw, 8, what)?,
            stride: u32_at(raw, 12, what)?,
            descriptor_words: u8_at(raw, 16, what)?,
            user_data: u8_at(raw, 17, what)?,
            slot: u8_at(raw, 18, what)?,
        });
    }

    let flags = u32_at(record, 0x14, what)?;
    let registers = u32_at(record, 0x50, what)?;
    Ok(Program {
        stage,
        wave: if flags & FLAG_WAVE32 != 0 {
            WaveSize::Wave32
        } else {
            WaveSize::Wave64
        },
        flags,
        vector_registers: (registers & 0x3FF) as u16,
        scalar_registers: (registers >> 20 & 0xFF) as u8,
        group_shared_bytes: u32_at(record, 0x4C, what)?,
        record_offset,
        code_offset,
        root_parameters,
        used_resources,
        code,
    })
}

/// Returns the bytes of one of a record's two tables of 20-byte entries. The
/// record holds the table's offset at `offset_at` and its entry count at
/// `count_at`.
fn table<'a>(
    payload: &'a [u8],
    record: &[u8],
    offset_at: usize,
    count_at: usize,
    what: &'static str,
) -> Result<&'a [u8], Error> {
    let count = u64::from(u32_at(record, count_at, "a stage record")?);
    if count == 0 {
        // An empty table's offset is not meaningful, so it is not checked.
        return Ok(&payload[..0]);
    }
    let offset = u64_at(record, offset_at, "a stage record")?;
    span(payload, offset, count * BINDING_ENTRY_SIZE as u64, what)
}
