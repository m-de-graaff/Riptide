//! Input, output and patch signatures (`ISG1`, `OSG1`, `PSG1`).
//!
//! A signature lists the values a shader stage receives or produces. The
//! three parts share one layout, the one the public DirectX shader compiler
//! writes:
//!
//! ```text
//! 0x00  number of elements, u32
//! 0x04  offset of the first element from the start of the part, u32
//! element, 32 bytes:
//!   0x00  output stream (geometry shaders), u32
//!   0x04  offset of the semantic name from the start of the part, u32
//!   0x08  semantic index, u32
//!   0x0C  system value, u32
//!   0x10  component type, u32
//!   0x14  register, u32
//!   0x18  mask of the components the element occupies, u8
//!   0x19  mask of the components always read (inputs) or never written
//!         (outputs), u8
//!   0x1A  padding, u16
//!   0x1C  minimum precision, u32
//! ```
//!
//! The semantic names are NUL-terminated strings stored after the elements.
//!
//! The layout is taken from the public compiler's headers. It has not yet
//! been compared field by field with the signatures of a real console shader.

use crate::Error;
use crate::bytes::{c_string, span, u8_at, u32_at};

/// The size of one element.
const ELEMENT_SIZE: usize = 32;

/// The type of each component of a signature element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentType {
    /// The type is not stated.
    Unknown,
    /// An unsigned 32-bit integer.
    UInt32,
    /// A signed 32-bit integer.
    SInt32,
    /// A 32-bit float.
    Float32,
    /// An unsigned 16-bit integer.
    UInt16,
    /// A signed 16-bit integer.
    SInt16,
    /// A 16-bit float.
    Float16,
    /// An unsigned 64-bit integer.
    UInt64,
    /// A signed 64-bit integer.
    SInt64,
    /// A 64-bit float.
    Float64,
    /// A number with no type assigned here.
    Other(u32),
}

impl ComponentType {
    /// Converts the number the signature stores.
    pub fn from_number(number: u32) -> Self {
        match number {
            0 => ComponentType::Unknown,
            1 => ComponentType::UInt32,
            2 => ComponentType::SInt32,
            3 => ComponentType::Float32,
            4 => ComponentType::UInt16,
            5 => ComponentType::SInt16,
            6 => ComponentType::Float16,
            7 => ComponentType::UInt64,
            8 => ComponentType::SInt64,
            9 => ComponentType::Float64,
            other => ComponentType::Other(other),
        }
    }
}

/// The system value of a signature element: what the pipeline, not the
/// shader, means by it. The number is Direct3D's `D3D_NAME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SystemValue(pub u32);

impl SystemValue {
    /// An ordinary value with no meaning to the pipeline.
    pub const NONE: SystemValue = SystemValue(0);
    /// The clip-space position.
    pub const POSITION: SystemValue = SystemValue(1);
    /// A colour output of a pixel shader.
    pub const TARGET: SystemValue = SystemValue(64);
    /// The depth output of a pixel shader.
    pub const DEPTH: SystemValue = SystemValue(65);

    /// Returns Direct3D's name for the value, or `None` for a number that has
    /// none.
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0 => "Undefined",
            1 => "Position",
            2 => "ClipDistance",
            3 => "CullDistance",
            4 => "RenderTargetArrayIndex",
            5 => "ViewportArrayIndex",
            6 => "VertexID",
            7 => "PrimitiveID",
            8 => "InstanceID",
            9 => "IsFrontFace",
            10 => "SampleIndex",
            11 => "FinalQuadEdgeTessFactor",
            12 => "FinalQuadInsideTessFactor",
            13 => "FinalTriEdgeTessFactor",
            14 => "FinalTriInsideTessFactor",
            15 => "FinalLineDetailTessFactor",
            16 => "FinalLineDensityTessFactor",
            23 => "Barycentrics",
            24 => "ShadingRate",
            25 => "CullPrimitive",
            64 => "Target",
            65 => "Depth",
            66 => "Coverage",
            67 => "DepthGreaterEqual",
            68 => "DepthLessEqual",
            69 => "StencilRef",
            70 => "InnerCoverage",
            _ => return None,
        })
    }
}

/// One value of a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureElement {
    /// The output stream, for geometry shader outputs; 0 otherwise.
    pub stream: u32,
    /// The semantic name, such as `TEXCOORD`.
    pub semantic_name: String,
    /// The semantic index, the 3 of `TEXCOORD3`.
    pub semantic_index: u32,
    /// What the pipeline means by the value.
    pub system_value: SystemValue,
    /// The type of each component.
    pub component_type: ComponentType,
    /// The register (row) the element is assigned. Elements that have no
    /// register, such as a pixel shader's depth output, hold `0xFFFF_FFFF`.
    pub register: u32,
    /// Which of the register's four components the element occupies: bit 0
    /// is x, bit 3 is w.
    pub mask: u8,
    /// For an input, the components the shader always reads. For an output,
    /// the components the shader never writes.
    pub read_write_mask: u8,
    /// The minimum precision the shader asked for; 0 is the default.
    pub min_precision: u32,
}

/// The elements of one signature part.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Signature {
    /// The elements in the order the part stores them.
    pub elements: Vec<SignatureElement>,
}

impl Signature {
    /// Reads a signature from the data of an `ISG1`, `OSG1` or `PSG1` part.
    pub fn parse(part: &[u8]) -> Result<Self, Error> {
        let count = u32_at(part, 0, "the signature header")? as usize;
        let first = u32_at(part, 4, "the signature header")?;
        let table_size = (count as u64) * ELEMENT_SIZE as u64;
        let table = span(part, u64::from(first), table_size, "the signature elements")?;
        let mut elements = Vec::with_capacity(count);
        for raw in table.chunks_exact(ELEMENT_SIZE) {
            let what = "a signature element";
            let name_at = u32_at(raw, 4, what)? as usize;
            elements.push(SignatureElement {
                stream: u32_at(raw, 0, what)?,
                semantic_name: c_string(part, name_at, "a semantic name")?,
                semantic_index: u32_at(raw, 8, what)?,
                system_value: SystemValue(u32_at(raw, 12, what)?),
                component_type: ComponentType::from_number(u32_at(raw, 16, what)?),
                register: u32_at(raw, 20, what)?,
                mask: u8_at(raw, 24, what)?,
                read_write_mask: u8_at(raw, 25, what)?,
                min_precision: u32_at(raw, 28, what)?,
            });
        }
        Ok(Signature { elements })
    }

    /// Returns the element assigned to `register` that occupies component
    /// `component` (0 for x to 3 for w), if any. Machine-code stage data names
    /// values by register, so this is how a translator finds what a row means.
    pub fn element_at(&self, register: u32, component: u8) -> Option<&SignatureElement> {
        let mask = 1u8.checked_shl(u32::from(component)).unwrap_or(0) & 0x0F;
        self.elements
            .iter()
            .find(|element| element.register == register && element.mask & mask != 0)
    }
}
