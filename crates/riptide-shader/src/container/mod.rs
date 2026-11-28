//! The DXBC container and what its parts hold.
//!
//! A shader is stored as a DXBC container: a fixed header, a table of
//! offsets, and one part per offset. Each part has a four-letter name and a
//! size.
//!
//! ```text
//! 0x00  "DXBC"
//! 0x04  digest, 16 bytes
//! 0x14  version: major u16, minor u16
//! 0x18  size of the whole container, u32
//! 0x1C  number of parts, u32
//! 0x20  offset of each part from the start of the container, u32 each
//! part  name (4 bytes), size u32, then that many bytes of data
//! ```
//!
//! [`Container::parse`] splits the container into its parts. The parts a
//! translator needs are then read on request:
//!
//! - `ISG1`, `OSG1`, `PSG1`: the input, output and per-patch or per-primitive
//!   signatures ([`signature`]).
//! - `PSV0`: pipeline state validation, which names the shader's kind and the
//!   resources it binds ([`psv`]).
//! - `DXIL`: on the console, the compiler's additions and the machine code
//!   ([`xbox`]).
//! - `SFI0` and `HASH`: the feature flags and the shader's hash, two small
//!   fixed layouts read here.
//!
//! The `PRIV` part is kept as bytes and not interpreted: nothing is known of
//! its contents. A container may carry an `ACCC` part in place of `DXIL` when
//! another compiler produced the machine code; that part is not read either.

pub mod psv;
pub mod signature;
pub mod stage;
pub mod xbox;

use crate::Error;
use crate::bytes::{span, u16_at, u32_at, u64_at};

pub use psv::PipelineValidation;
pub use signature::Signature;
pub use xbox::XboxDxil;

/// The four-letter name of a container part.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc(pub [u8; 4]);

impl FourCc {
    /// The program itself: bitcode on a PC, the console's additions on the console.
    pub const DXIL: FourCc = FourCc(*b"DXIL");
    /// The input signature.
    pub const ISG1: FourCc = FourCc(*b"ISG1");
    /// The output signature.
    pub const OSG1: FourCc = FourCc(*b"OSG1");
    /// The per-patch signature of tessellation shaders and the per-primitive
    /// signature of mesh shaders.
    pub const PSG1: FourCc = FourCc(*b"PSG1");
    /// Pipeline state validation.
    pub const PSV0: FourCc = FourCc(*b"PSV0");
    /// The optional features the shader uses.
    pub const SFI0: FourCc = FourCc(*b"SFI0");
    /// The hash of the shader.
    pub const HASH: FourCc = FourCc(*b"HASH");
    /// Private data of whoever compiled the shader.
    pub const PRIV: FourCc = FourCc(*b"PRIV");
}

impl std::fmt::Display for FourCc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for &byte in &self.0 {
            let shown = if byte.is_ascii_graphic() {
                byte as char
            } else {
                '.'
            };
            write!(f, "{shown}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for FourCc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FourCc({self})")
    }
}

/// One part of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part<'a> {
    /// The part's four-letter name.
    pub name: FourCc,
    /// Where the part's header starts in the container.
    pub offset: usize,
    /// The part's data, without its eight-byte header.
    pub data: &'a [u8],
}

/// What kind of shader a program is, as the program header and the pipeline
/// state validation part number it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShaderKind {
    /// A pixel shader.
    Pixel,
    /// A vertex shader.
    Vertex,
    /// A geometry shader.
    Geometry,
    /// A hull shader, the first tessellation stage.
    Hull,
    /// A domain shader, the second tessellation stage.
    Domain,
    /// A compute shader.
    Compute,
    /// A library of functions, as ray tracing uses.
    Library,
    /// A ray generation shader.
    RayGeneration,
    /// A ray intersection shader.
    Intersection,
    /// A ray any-hit shader.
    AnyHit,
    /// A ray closest-hit shader.
    ClosestHit,
    /// A ray miss shader.
    Miss,
    /// A callable shader.
    Callable,
    /// A mesh shader.
    Mesh,
    /// An amplification shader.
    Amplification,
    /// A number with no kind assigned here.
    Other(u16),
}

impl ShaderKind {
    /// Converts the number the formats store.
    pub fn from_number(number: u16) -> Self {
        match number {
            0 => ShaderKind::Pixel,
            1 => ShaderKind::Vertex,
            2 => ShaderKind::Geometry,
            3 => ShaderKind::Hull,
            4 => ShaderKind::Domain,
            5 => ShaderKind::Compute,
            6 => ShaderKind::Library,
            7 => ShaderKind::RayGeneration,
            8 => ShaderKind::Intersection,
            9 => ShaderKind::AnyHit,
            10 => ShaderKind::ClosestHit,
            11 => ShaderKind::Miss,
            12 => ShaderKind::Callable,
            13 => ShaderKind::Mesh,
            14 => ShaderKind::Amplification,
            other => ShaderKind::Other(other),
        }
    }
}

/// The contents of the `HASH` part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderHash {
    /// Flags; bit 0 says the hash covers the source as well as the program.
    pub flags: u32,
    /// The hash.
    pub digest: [u8; 16],
}

/// A container split into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container<'a> {
    /// The digest in the header. It is not checked.
    pub digest: [u8; 16],
    /// The container version, major then minor.
    pub version: (u16, u16),
    /// The parts in the order of the offset table.
    pub parts: Vec<Part<'a>>,
}

/// The size of the fixed header, up to the offset table.
const HEADER_SIZE: usize = 0x20;
/// The size of a part's header: its name and its size.
const PART_HEADER_SIZE: usize = 8;

impl<'a> Container<'a> {
    /// Splits a container into its parts. Bytes after the size the header
    /// states are ignored.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        if data.get(..4) != Some(b"DXBC") {
            return Err(Error::NotDxbc);
        }
        let mut digest = [0u8; 16];
        digest.copy_from_slice(span(data, 4, 16, "the container header")?);
        let version = (
            u16_at(data, 0x14, "the container header")?,
            u16_at(data, 0x16, "the container header")?,
        );
        let total = u32_at(data, 0x18, "the container header")? as usize;
        let count = u32_at(data, 0x1C, "the container header")? as usize;
        if total < HEADER_SIZE {
            return Err(Error::Malformed(
                "the container size is smaller than its header",
            ));
        }
        let data = data.get(..total).ok_or(Error::Truncated("the container"))?;
        // Every part needs an offset and a header, so a count the data cannot
        // hold is rejected before anything is allocated for it.
        if count > (total - HEADER_SIZE) / (4 + PART_HEADER_SIZE) {
            return Err(Error::Malformed(
                "the part count is larger than the container",
            ));
        }
        let mut parts = Vec::with_capacity(count);
        for index in 0..count {
            let offset = u32_at(data, HEADER_SIZE + 4 * index, "the part offset table")? as usize;
            let header = span(
                data,
                offset as u64,
                PART_HEADER_SIZE as u64,
                "a part header",
            )?;
            let name = FourCc([header[0], header[1], header[2], header[3]]);
            let size = u32_at(header, 4, "a part header")?;
            let body = span(
                data,
                offset as u64 + PART_HEADER_SIZE as u64,
                u64::from(size),
                "a part's data",
            )?;
            parts.push(Part {
                name,
                offset,
                data: body,
            });
        }
        Ok(Container {
            digest,
            version,
            parts,
        })
    }

    /// Returns the first part with the given name.
    pub fn part(&self, name: FourCc) -> Option<&Part<'a>> {
        self.parts.iter().find(|part| part.name == name)
    }

    /// Reads the signature in the part with the given name (`ISG1`, `OSG1` or
    /// `PSG1`), or `None` if the container has no such part.
    pub fn signature(&self, name: FourCc) -> Result<Option<Signature>, Error> {
        self.part(name)
            .map(|part| Signature::parse(part.data))
            .transpose()
    }

    /// Reads the input signature, if there is one.
    pub fn input_signature(&self) -> Result<Option<Signature>, Error> {
        self.signature(FourCc::ISG1)
    }

    /// Reads the output signature, if there is one.
    pub fn output_signature(&self) -> Result<Option<Signature>, Error> {
        self.signature(FourCc::OSG1)
    }

    /// Reads the per-patch or per-primitive signature, if there is one.
    pub fn patch_signature(&self) -> Result<Option<Signature>, Error> {
        self.signature(FourCc::PSG1)
    }

    /// Reads the pipeline state validation part, if there is one.
    pub fn pipeline_validation(&self) -> Result<Option<PipelineValidation>, Error> {
        self.part(FourCc::PSV0)
            .map(|part| PipelineValidation::parse(part.data))
            .transpose()
    }

    /// Reads the `DXIL` part, if there is one: its program header and, on the
    /// console, the table of blobs after it.
    pub fn xbox_dxil(&self) -> Result<Option<XboxDxil<'a>>, Error> {
        self.part(FourCc::DXIL)
            .map(|part| XboxDxil::parse(part.data))
            .transpose()
    }

    /// Returns the 64 feature flag bits of the `SFI0` part, if the part is
    /// there and long enough.
    pub fn feature_flags(&self) -> Option<u64> {
        let part = self.part(FourCc::SFI0)?;
        u64_at(part.data, 0, "the feature flags").ok()
    }

    /// Returns the contents of the `HASH` part, if the part is there and long
    /// enough.
    pub fn shader_hash(&self) -> Option<ShaderHash> {
        let part = self.part(FourCc::HASH)?;
        let flags = u32_at(part.data, 0, "the shader hash").ok()?;
        let mut digest = [0u8; 16];
        digest.copy_from_slice(span(part.data, 4, 16, "the shader hash").ok()?);
        Some(ShaderHash { flags, digest })
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
