//! Pipeline state validation (`PSV0`).
//!
//! The compiler writes this part so that a runtime can check a shader against
//! a pipeline without reading the program. For a translator it is the one
//! place that states, outside the program, what kind of shader this is, which
//! resources it binds, and what each signature row means.
//!
//! The layout is the public DirectX shader compiler's. It grew over time, so
//! the part states the size of its own records and a reader takes the fields
//! it knows:
//!
//! ```text
//! u32   size of the runtime info that follows
//! runtime info:
//!   0x00  16 bytes of facts about the stage, laid out per kind of shader
//!   0x10  least wave lane count the shader expects, u32
//!   0x14  greatest wave lane count the shader expects, u32
//!   -- from version 1 (36 bytes) --
//!   0x18  kind of shader, u8
//!   0x19  whether the shader uses the view id, u8
//!   0x1A  two bytes whose meaning depends on the kind
//!   0x1C  input elements, output elements, patch or primitive elements, u8 each
//!   0x1F  input vectors, u8, then output vectors of each of 4 streams, u8 each
//!   -- from version 2 (48 bytes) --
//!   0x24  thread group size x, y, z, u32 each
//!   -- from version 3 (52 bytes) --
//!   0x30  offset of the entry point's name in the string table, u32
//! u32   number of resource bindings
//! u32   size of one binding (only if the number is not zero)
//! binding:
//!   0x00  resource type, u32
//!   0x04  register space, u32
//!   0x08  first register, u32
//!   0x0C  last register, u32
//!   -- from version 1 (24 bytes) --
//!   0x10  resource kind (the shape of the resource), u32
//!   0x14  flags, u32
//! -- everything below only from runtime info version 1 --
//! u32   size of the string table, then the table (NUL-terminated strings)
//! u32   number of entries of the semantic index table, then u32 entries
//! u32   size of one signature element (only if there are any elements)
//! signature elements: inputs, then outputs, then patch or primitive elements
//!   0x00  offset of the semantic name in the string table, u32
//!   0x04  first entry in the semantic index table, u32 (one entry per row)
//!   0x08  rows, u8
//!   0x09  first row (register), u8
//!   0x0A  bits 0-3 columns, bits 4-5 first column, bit 6 allocated, u8
//!   0x0B  semantic kind, u8
//!   0x0C  component type, u8
//!   0x0D  interpolation mode, u8
//!   0x0E  bits 0-3 dynamic index mask, bits 4-5 output stream, u8
//!   0x0F  reserved, u8
//! ```
//!
//! After the signature elements the part holds tables that say which outputs
//! depend on which inputs and on the view id. A translator has no use for
//! them and they are not read. The offset where they start is kept in
//! [`PipelineValidation::dependency_tables_offset`].
//!
//! The layout was written from the public compiler's headers. It has not yet
//! been compared field by field with a `PSV0` part of a real console shader.

use super::ShaderKind;
use crate::Error;
use crate::bytes::{c_string, span, u8_at, u16_at, u32_at};

/// The size of the first version of the runtime info.
const INFO_V0: usize = 24;
/// The size of the runtime info from which the kind and the signature counts exist.
const INFO_V1: usize = 36;
/// The size of the runtime info from which the thread group size exists.
const INFO_V2: usize = 48;
/// The size of the runtime info from which the entry point name exists.
const INFO_V3: usize = 52;
/// The size of the first version of a resource binding.
const BINDING_V0: usize = 16;
/// The size of a resource binding from which the kind and flags exist.
const BINDING_V1: usize = 24;
/// The size of the first version of a signature element.
const ELEMENT_V0: usize = 16;

/// What a bound resource is, as far as binding goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceType {
    /// The binding is not valid.
    Invalid,
    /// A sampler.
    Sampler,
    /// A constant buffer.
    ConstantBuffer,
    /// A read-only texture or typed buffer.
    SrvTyped,
    /// A read-only raw (byte address) buffer.
    SrvRaw,
    /// A read-only structured buffer.
    SrvStructured,
    /// A writable texture or typed buffer.
    UavTyped,
    /// A writable raw (byte address) buffer.
    UavRaw,
    /// A writable structured buffer.
    UavStructured,
    /// A writable structured buffer with a hidden counter.
    UavStructuredWithCounter,
    /// A number with no type assigned here.
    Other(u32),
}

impl ResourceType {
    /// Converts the number the part stores.
    pub fn from_number(number: u32) -> Self {
        match number {
            0 => ResourceType::Invalid,
            1 => ResourceType::Sampler,
            2 => ResourceType::ConstantBuffer,
            3 => ResourceType::SrvTyped,
            4 => ResourceType::SrvRaw,
            5 => ResourceType::SrvStructured,
            6 => ResourceType::UavTyped,
            7 => ResourceType::UavRaw,
            8 => ResourceType::UavStructured,
            9 => ResourceType::UavStructuredWithCounter,
            other => ResourceType::Other(other),
        }
    }
}

/// One range of registers the shader binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceBinding {
    /// What is bound.
    pub resource_type: ResourceType,
    /// The register space.
    pub space: u32,
    /// The first register of the range.
    pub lower_bound: u32,
    /// The last register of the range; `0xFFFF_FFFF` for an unbounded array.
    pub upper_bound: u32,
    /// The shape of the resource (texture dimension, kind of buffer) as the
    /// compiler numbers it, when the part is new enough to state it.
    pub resource_kind: Option<u32>,
    /// Flags, when the part is new enough to state them.
    pub flags: Option<u32>,
}

/// One row range of a signature as this part describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationElement {
    /// The semantic name. System values often have an empty name here and
    /// are told apart by [`ValidationElement::semantic_kind`].
    pub semantic_name: String,
    /// The semantic index of each row.
    pub semantic_indexes: Vec<u32>,
    /// How many rows the element occupies.
    pub rows: u8,
    /// The first row (register).
    pub start_row: u8,
    /// How many columns (components) the element occupies.
    pub columns: u8,
    /// The first column.
    pub start_column: u8,
    /// Whether the element was given a register at all.
    pub allocated: bool,
    /// The semantic kind as the compiler numbers it; 0 is an ordinary value.
    pub semantic_kind: u8,
    /// The component type as the compiler numbers it.
    pub component_type: u8,
    /// The interpolation mode as the compiler numbers it.
    pub interpolation_mode: u8,
    /// Which rows are indexed dynamically, one bit per row.
    pub dynamic_index_mask: u8,
    /// The output stream, for geometry shader outputs.
    pub output_stream: u8,
}

/// The stage facts of the first 16 bytes, read according to the kind of
/// shader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageDetails {
    /// A vertex shader.
    Vertex {
        /// Whether the shader writes a position.
        output_position: bool,
    },
    /// A pixel shader.
    Pixel {
        /// Whether the shader writes depth.
        depth_output: bool,
        /// Whether the shader runs once per sample.
        sample_frequency: bool,
    },
    /// A geometry shader.
    Geometry {
        /// The input primitive as the compiler numbers it.
        input_primitive: u32,
        /// The output topology as the compiler numbers it.
        output_topology: u32,
        /// Which output streams the shader writes, one bit each.
        output_stream_mask: u32,
        /// Whether the shader writes a position.
        output_position: bool,
    },
    /// A hull shader.
    Hull {
        /// How many control points come in.
        input_control_points: u32,
        /// How many control points go out.
        output_control_points: u32,
        /// The tessellator domain as the compiler numbers it.
        domain: u32,
        /// The tessellator output primitive as the compiler numbers it.
        output_primitive: u32,
    },
    /// A domain shader.
    Domain {
        /// How many control points come in.
        input_control_points: u32,
        /// Whether the shader writes a position.
        output_position: bool,
        /// The tessellator domain as the compiler numbers it.
        domain: u32,
    },
    /// A mesh shader.
    Mesh {
        /// Bytes of group-shared memory the shader uses.
        group_shared_bytes: u32,
        /// Bytes of the payload it receives from the amplification shader.
        payload_bytes: u32,
        /// The most vertices one group outputs.
        max_vertices: u16,
        /// The most primitives one group outputs.
        max_primitives: u16,
    },
    /// An amplification shader.
    Amplification {
        /// Bytes of the payload it passes to the mesh shader.
        payload_bytes: u32,
    },
    /// A kind whose 16 bytes hold nothing, or a part too old to state the kind.
    None,
}

/// The contents of a `PSV0` part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineValidation {
    /// The size of the runtime info, which tells how new the part is.
    pub info_size: u32,
    /// The 16 bytes of stage facts as stored. [`PipelineValidation::stage_details`]
    /// reads them.
    pub stage_info: [u8; 16],
    /// The least wave lane count the shader expects.
    pub min_wave_lanes: u32,
    /// The greatest wave lane count the shader expects.
    pub max_wave_lanes: u32,
    /// The kind of shader, when the part is new enough to state it.
    pub kind: Option<ShaderKind>,
    /// Whether the shader uses the view id.
    pub uses_view_id: bool,
    /// The two bytes whose meaning depends on the kind: the greatest vertex
    /// count of a geometry shader, or the patch or primitive vector count
    /// (low byte) and mesh output topology (high byte).
    pub kind_word: u16,
    /// How many vectors (rows) the inputs occupy.
    pub input_vectors: u8,
    /// How many vectors (rows) the outputs occupy, for each of 4 streams.
    pub output_vectors: [u8; 4],
    /// The thread group size of a compute, mesh or amplification shader,
    /// when the part is new enough to state it.
    pub thread_group_size: Option<[u32; 3]>,
    /// The name of the entry point, when the part is new enough to state it.
    pub entry_name: Option<String>,
    /// The resources the shader binds.
    pub resources: Vec<ResourceBinding>,
    /// The input signature rows.
    pub inputs: Vec<ValidationElement>,
    /// The output signature rows.
    pub outputs: Vec<ValidationElement>,
    /// The per-patch or per-primitive signature rows.
    pub patch_or_primitive: Vec<ValidationElement>,
    /// Where the dependency tables, which are not read, start in the part.
    pub dependency_tables_offset: usize,
}

impl PipelineValidation {
    /// Reads the data of a `PSV0` part.
    pub fn parse(part: &[u8]) -> Result<Self, Error> {
        let info_size = u32_at(part, 0, "the validation header")?;
        let info = span(part, 4, u64::from(info_size), "the validation runtime info")?;
        if info.len() < INFO_V0 {
            return Err(Error::Malformed("the validation runtime info is too small"));
        }
        let what = "the validation runtime info";
        let mut stage_info = [0u8; 16];
        stage_info.copy_from_slice(&info[..16]);
        let mut out = PipelineValidation {
            info_size,
            stage_info,
            min_wave_lanes: u32_at(info, 16, what)?,
            max_wave_lanes: u32_at(info, 20, what)?,
            kind: None,
            uses_view_id: false,
            kind_word: 0,
            input_vectors: 0,
            output_vectors: [0; 4],
            thread_group_size: None,
            entry_name: None,
            resources: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            patch_or_primitive: Vec::new(),
            dependency_tables_offset: 0,
        };
        let mut element_counts = [0usize; 3];
        let has_v1 = info.len() >= INFO_V1;
        if has_v1 {
            out.kind = Some(ShaderKind::from_number(u16::from(info[24])));
            out.uses_view_id = info[25] != 0;
            out.kind_word = u16_at(info, 26, what)?;
            element_counts = [info[28].into(), info[29].into(), info[30].into()];
            out.input_vectors = info[31];
            out.output_vectors = [info[32], info[33], info[34], info[35]];
        }
        if info.len() >= INFO_V2 {
            out.thread_group_size = Some([
                u32_at(info, 36, what)?,
                u32_at(info, 40, what)?,
                u32_at(info, 44, what)?,
            ]);
        }
        let entry_name_at = if info.len() >= INFO_V3 {
            Some(u32_at(info, 48, what)?)
        } else {
            None
        };

        let mut at = 4 + info.len();
        let binding_count = u32_at(part, at, "the resource binding count")? as usize;
        at += 4;
        if binding_count > 0 {
            let size = u32_at(part, at, "the resource binding size")? as usize;
            at += 4;
            if size < BINDING_V0 {
                return Err(Error::Malformed("a resource binding is too small"));
            }
            let table = span(
                part,
                at as u64,
                binding_count as u64 * size as u64,
                "the resource bindings",
            )?;
            at += table.len();
            let what = "a resource binding";
            for raw in table.chunks_exact(size) {
                let newer = size >= BINDING_V1;
                out.resources.push(ResourceBinding {
                    resource_type: ResourceType::from_number(u32_at(raw, 0, what)?),
                    space: u32_at(raw, 4, what)?,
                    lower_bound: u32_at(raw, 8, what)?,
                    upper_bound: u32_at(raw, 12, what)?,
                    resource_kind: if newer {
                        Some(u32_at(raw, 16, what)?)
                    } else {
                        None
                    },
                    flags: if newer {
                        Some(u32_at(raw, 20, what)?)
                    } else {
                        None
                    },
                });
            }
        }
        if !has_v1 {
            out.dependency_tables_offset = at;
            return Ok(out);
        }

        let strings_size = u32_at(part, at, "the validation string table size")?;
        at += 4;
        let strings = span(
            part,
            at as u64,
            u64::from(strings_size),
            "the validation string table",
        )?;
        at += strings.len();
        if let Some(name_at) = entry_name_at {
            out.entry_name = c_string(strings, name_at as usize, "the entry point name").ok();
        }

        let index_count = u32_at(part, at, "the semantic index table size")?;
        at += 4;
        let index_table = span(
            part,
            at as u64,
            u64::from(index_count) * 4,
            "the semantic index table",
        )?;
        at += index_table.len();

        if element_counts.iter().sum::<usize>() > 0 {
            let size = u32_at(part, at, "the validation element size")? as usize;
            at += 4;
            if size < ELEMENT_V0 {
                return Err(Error::Malformed("a validation element is too small"));
            }
            let lists = [
                &mut out.inputs,
                &mut out.outputs,
                &mut out.patch_or_primitive,
            ];
            for (list, count) in lists.into_iter().zip(element_counts) {
                let table = span(
                    part,
                    at as u64,
                    count as u64 * size as u64,
                    "the validation elements",
                )?;
                at += table.len();
                for raw in table.chunks_exact(size) {
                    list.push(element(raw, strings, index_table)?);
                }
            }
        }
        out.dependency_tables_offset = at;
        Ok(out)
    }

    /// Reads the 16 bytes of stage facts according to the kind of shader.
    pub fn stage_details(&self) -> StageDetails {
        let raw = &self.stage_info;
        let word = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        match self.kind {
            Some(ShaderKind::Vertex) => StageDetails::Vertex {
                output_position: raw[0] != 0,
            },
            Some(ShaderKind::Pixel) => StageDetails::Pixel {
                depth_output: raw[0] != 0,
                sample_frequency: raw[1] != 0,
            },
            Some(ShaderKind::Geometry) => StageDetails::Geometry {
                input_primitive: word(0),
                output_topology: word(4),
                output_stream_mask: word(8),
                output_position: raw[12] != 0,
            },
            Some(ShaderKind::Hull) => StageDetails::Hull {
                input_control_points: word(0),
                output_control_points: word(4),
                domain: word(8),
                output_primitive: word(12),
            },
            Some(ShaderKind::Domain) => StageDetails::Domain {
                input_control_points: word(0),
                output_position: raw[4] != 0,
                domain: word(8),
            },
            Some(ShaderKind::Mesh) => StageDetails::Mesh {
                group_shared_bytes: word(0),
                payload_bytes: word(8),
                max_vertices: u16::from_le_bytes([raw[12], raw[13]]),
                max_primitives: u16::from_le_bytes([raw[14], raw[15]]),
            },
            Some(ShaderKind::Amplification) => StageDetails::Amplification {
                payload_bytes: word(0),
            },
            _ => StageDetails::None,
        }
    }
}

/// Reads one signature element, resolving its name and its semantic indexes.
fn element(raw: &[u8], strings: &[u8], index_table: &[u8]) -> Result<ValidationElement, Error> {
    let what = "a validation element";
    let name_at = u32_at(raw, 0, what)? as usize;
    let first_index = u32_at(raw, 4, what)?;
    let rows = u8_at(raw, 8, what)?;
    let packed = u8_at(raw, 10, what)?;
    let stream = u8_at(raw, 14, what)?;
    let indexes = span(
        index_table,
        u64::from(first_index) * 4,
        u64::from(rows) * 4,
        "an element's semantic indexes",
    )?;
    Ok(ValidationElement {
        // A name is only ever shown, and parts exist whose unnamed elements
        // point at a string table that is empty, so a name that cannot be
        // read is taken as empty instead of failing the whole part.
        semantic_name: c_string(strings, name_at, "a validation semantic name").unwrap_or_default(),
        semantic_indexes: indexes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        rows,
        start_row: u8_at(raw, 9, what)?,
        columns: packed & 0x0F,
        start_column: packed >> 4 & 3,
        allocated: packed & 0x40 != 0,
        semantic_kind: u8_at(raw, 11, what)?,
        component_type: u8_at(raw, 12, what)?,
        interpolation_mode: u8_at(raw, 13, what)?,
        dynamic_index_mask: stream & 0x0F,
        output_stream: stream >> 4 & 3,
    })
}
