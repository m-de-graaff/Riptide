//! Host pipelines: a title's pipeline with translated shaders in place of its programs.
//!
//! A title describes a pipeline as a stream of subobjects in PC's own format, with a few
//! numbers of the console's own. The host pipeline is made from the same stream: the fixed
//! state (blending, the rasterizer, depth and stencil, the target formats) is copied as it
//! is, the root signature becomes the one host root signature (`stage.rs`), the programs
//! become compiled translations, and the vertex layout is pointed at this crate's copy of
//! it. What the host has no use for (stream output, a cached pipeline) is left out.

// Nothing draws with translated shaders yet; a test on a real GPU does (`hosttest.rs`).
#![allow(dead_code)]

use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMPUTE_PIPELINE_STATE_DESC, D3D12_PIPELINE_STATE_STREAM_DESC, D3D12_SHADER_BYTECODE,
    ID3D12Device2, ID3D12PipelineState,
};
use windows::core::Interface;

use crate::pipeline::{Element, Subobject};

/// Compiled host shaders for the stages of one pipeline.
#[derive(Default)]
pub(crate) struct Shaders {
    pub(crate) vertex: Option<Vec<u8>>,
    pub(crate) pixel: Option<Vec<u8>>,
    /// A geometry shader of Riptide's own, for a topology the host does not have.
    pub(crate) geometry: Option<Vec<u8>>,
}

/// The console's topology types the host does not have: rectangles and quads. Both are
/// drawn as triangles, with a geometry shader that completes each.
const RECTANGLES: u32 = 19;
const QUADS: u32 = 32;
/// The host's triangle topology type.
const TRIANGLES: u32 = 3;

/// The subobject types this module writes or leaves out.
const ROOT: u32 = 0;
const VERTEX: u32 = 1;
const PIXEL: u32 = 2;
const GEOMETRY: u32 = 5;
const LAYOUT: u32 = 12;
const TOPOLOGY: u32 = 14;
const TARGET_FORMATS: u32 = 15;
const DEPTH_FORMAT: u32 = 16;

/// Whether a title's stream asks for a topology type the host does not have, which a
/// geometry shader has to complete.
pub(crate) fn completes_shapes(stream: &[u8], subobjects: &[Subobject]) -> bool {
    subobjects
        .iter()
        .find(|one| one.kind == TOPOLOGY)
        .is_some_and(|one| matches!(word(stream, one.offset + 4), RECTANGLES | QUADS))
}

fn word(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |bytes| {
        u32::from_le_bytes(bytes.try_into().expect("4 bytes"))
    })
}

fn put_word(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Appends a subobject that is a type and a pointer with a size or count: 24 bytes.
fn push_pointer(stream: &mut Vec<u8>, kind: u32, pointer: usize, count: usize) {
    stream.extend_from_slice(&u64::from(kind).to_le_bytes());
    stream.extend_from_slice(&(pointer as u64).to_le_bytes());
    stream.extend_from_slice(&(count as u64).to_le_bytes());
}

/// `D3D12_INPUT_ELEMENT_DESC`.
#[repr(C)]
struct HostElement {
    name: *const std::ffi::c_char,
    index: u32,
    format: u32,
    slot: u32,
    offset: u32,
    class: u32,
    step: u32,
}

/// The host stream for a title's `stream`, and the host vertex layout it points into,
/// which has to live as long as the stream is used.
///
/// A subobject that names a program the host has no translation for is an error, said by
/// the subobject's type.
fn host_stream(
    stream: &[u8],
    subobjects: &[Subobject],
    layout: &[Element],
    root: usize,
    shaders: &Shaders,
) -> Result<(Vec<u8>, Vec<HostElement>), String> {
    let elements: Vec<HostElement> = layout
        .iter()
        .map(|element| HostElement {
            name: element.name.as_ptr(),
            index: element.index,
            format: crate::target::host_format(element.format)
                .map_or(element.format, |format| format.0 as u32),
            slot: element.slot,
            offset: element.offset,
            class: element.class,
            step: element.step,
        })
        .collect();
    let mut host = Vec::with_capacity(stream.len() + 64);
    let mut geometry_written = false;
    let program = |host: &mut Vec<u8>, kind: u32, code: &Option<Vec<u8>>| {
        if let Some(code) = code {
            push_pointer(host, kind, code.as_ptr() as usize, code.len());
        }
    };
    for one in subobjects {
        let bytes = &stream[one.offset..one.offset + one.bytes];
        match one.kind {
            ROOT => {
                host.extend_from_slice(&u64::from(ROOT).to_le_bytes());
                host.extend_from_slice(&(root as u64).to_le_bytes());
            }
            VERTEX => program(&mut host, VERTEX, &shaders.vertex),
            PIXEL => program(&mut host, PIXEL, &shaders.pixel),
            GEOMETRY => {
                if word(bytes, 16) != 0 || word(bytes, 20) != 0 {
                    return Err("the pipeline has a geometry program of its own".to_owned());
                }
                program(&mut host, GEOMETRY, &shaders.geometry);
                geometry_written = true;
            }
            // Hull, domain, amplification and mesh programs: none is translated yet. A
            // subobject with no program in it is left out.
            3 | 4 | 23 | 24 => {
                if word(bytes, 16) != 0 || word(bytes, 20) != 0 {
                    return Err(format!(
                        "the pipeline has a program of subobject type {}, which is not \
                         translated yet",
                        one.kind
                    ));
                }
            }
            // A compute program is no part of a drawing pipeline.
            6 => {}
            LAYOUT => {
                if !elements.is_empty() {
                    push_pointer(
                        &mut host,
                        LAYOUT,
                        elements.as_ptr() as usize,
                        elements.len(),
                    );
                }
            }
            TOPOLOGY => {
                let at = host.len();
                host.extend_from_slice(bytes);
                if matches!(word(bytes, 4), RECTANGLES | QUADS) {
                    put_word(&mut host, at + 4, TRIANGLES);
                }
            }
            TARGET_FORMATS => {
                let at = host.len();
                host.extend_from_slice(bytes);
                for index in 0..8 {
                    let place = at + 4 + 4 * index;
                    if let Some(format) = crate::target::host_format(word(&host, place)) {
                        put_word(&mut host, place, format.0 as u32);
                    }
                }
            }
            DEPTH_FORMAT => {
                let at = host.len();
                host.extend_from_slice(bytes);
                if let Some(format) = crate::target::host_format(word(bytes, 4)) {
                    put_word(&mut host, at + 4, format.0 as u32);
                }
            }
            // Stream output and a cached pipeline: the host is given neither.
            7 | 19 => {}
            _ => host.extend_from_slice(bytes),
        }
    }
    if !geometry_written {
        program(&mut host, GEOMETRY, &shaders.geometry);
    }
    Ok((host, elements))
}

/// Makes the host pipeline for a title's drawing pipeline, described by its `stream`, with
/// `shaders` for its programs.
pub(crate) fn drawing(
    stream: &[u8],
    subobjects: &[Subobject],
    layout: &[Element],
    shaders: &Shaders,
) -> Result<ID3D12PipelineState, String> {
    let host = crate::host::host().map_err(|code| format!("no host device ({code:#x})"))?;
    let root = crate::stage::root_signature().ok_or("no host root signature")?;
    let (bytes, _elements) =
        host_stream(stream, subobjects, layout, root.as_raw() as usize, shaders)?;
    let device: ID3D12Device2 = host
        .device
        .cast()
        .map_err(|error| format!("the host device makes no pipelines from streams: {error}"))?;
    let description = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: bytes.len(),
        pPipelineStateSubobjectStream: bytes.as_ptr().cast_mut().cast(),
    };
    // SAFETY: a plain call on the host device. The stream, the layout it points into, the
    // shaders and the root signature all outlive it.
    unsafe { device.CreatePipelineState(&description) }
        .map_err(|error| format!("the host refused the pipeline: {error}"))
}

/// Makes the host pipeline for a compute program compiled as `shader`.
pub(crate) fn compute(shader: &[u8]) -> Result<ID3D12PipelineState, String> {
    let host = crate::host::host().map_err(|code| format!("no host device ({code:#x})"))?;
    let root = crate::stage::root_signature().ok_or("no host root signature")?;
    let description = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
        CS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: shader.as_ptr().cast(),
            BytecodeLength: shader.len(),
        },
        ..Default::default()
    };
    // SAFETY: a plain call on the host device; the shader and the root signature outlive
    // it.
    let made = unsafe { host.device.CreateComputePipelineState(&description) };
    drop(std::mem::ManuallyDrop::into_inner(
        description.pRootSignature,
    ));
    made.map_err(|error| format!("the host refused the pipeline: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subobject(kind: u32, offset: usize, bytes: usize) -> Subobject {
        Subobject {
            kind,
            offset,
            bytes,
        }
    }

    /// A stream of: a root signature, a vertex program, an empty hull program, a topology
    /// of rectangles, target formats, and a cached pipeline.
    fn stream() -> (Vec<u8>, Vec<Subobject>) {
        let mut stream = Vec::new();
        let mut subobjects = Vec::new();
        let mut add = |kind: u32, payload: &[u8]| {
            let offset = stream.len();
            stream.extend_from_slice(&kind.to_le_bytes());
            stream.extend_from_slice(payload);
            while stream.len() % 8 != 0 {
                stream.push(0);
            }
            subobjects.push(subobject(kind, offset, stream.len() - offset));
        };
        add(ROOT, &[0; 12]);
        add(VERTEX, &[0xaa; 20]);
        add(4, &[0; 20]);
        add(TOPOLOGY, &RECTANGLES.to_le_bytes());
        let mut formats = [0u8; 36];
        formats[..4].copy_from_slice(&28u32.to_le_bytes());
        formats[4..8].copy_from_slice(&118u32.to_le_bytes());
        formats[32..].copy_from_slice(&2u32.to_le_bytes());
        add(TARGET_FORMATS, &formats);
        add(19, &[0x55; 20]);
        (stream, subobjects)
    }

    #[test]
    fn the_host_stream_has_the_hosts_programs_root_and_numbers() {
        let (stream, subobjects) = stream();
        let shaders = Shaders {
            vertex: Some(vec![1, 2, 3]),
            pixel: None,
            geometry: Some(vec![4, 5]),
        };
        let (host, elements) = host_stream(&stream, &subobjects, &[], 0x1234, &shaders).unwrap();
        assert!(elements.is_empty());
        // The root signature, the vertex shader, the topology, the formats, and the
        // geometry shader added at the end. The hull program and cached pipeline are gone.
        assert_eq!(host.len(), 16 + 24 + 8 + 40 + 24);
        assert_eq!(word(&host, 8), 0x1234);
        assert_eq!(word(&host, 16), VERTEX);
        let vertex = shaders.vertex.as_ref().unwrap();
        assert_eq!(
            u64::from_le_bytes(host[24..32].try_into().unwrap()),
            vertex.as_ptr() as u64
        );
        assert_eq!(word(&host, 32), 3);
        // Rectangles are drawn as triangles, and the console's depth format has a host one.
        assert_eq!((word(&host, 40), word(&host, 44)), (TOPOLOGY, TRIANGLES));
        assert_eq!((word(&host, 52), word(&host, 56)), (28, 45));
        assert_eq!(word(&host, 88), GEOMETRY);
        assert!(completes_shapes(&stream, &subobjects));
    }

    #[test]
    fn a_program_with_no_translation_is_an_error_that_names_it() {
        let (mut stream, subobjects) = stream();
        // The hull program's size, 16 bytes into its subobject.
        let hull = subobjects[2].offset;
        stream[hull + 16] = 9;
        let error = host_stream(&stream, &subobjects, &[], 0, &Shaders::default())
            .err()
            .unwrap();
        assert!(error.contains("type 4"), "{error}");
    }

    #[test]
    fn the_layout_is_this_crates_copy_with_host_formats() {
        let layout = [Element {
            name: std::ffi::CString::new("POSITION").unwrap(),
            index: 0,
            format: 6,
            slot: 0,
            offset: 0,
            class: 0,
            step: 0,
        }];
        let stream = [12u32.to_le_bytes(), [0; 4], [0; 4], [0; 4], [0; 4], [0; 4]].concat();
        let subobjects = [subobject(LAYOUT, 0, 24)];
        let (host, elements) =
            host_stream(&stream, &subobjects, &layout, 0, &Shaders::default()).unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].name, layout[0].name.as_ptr());
        assert_eq!(
            u64::from_le_bytes(host[8..16].try_into().unwrap()),
            elements.as_ptr() as u64
        );
        assert_eq!(word(&host, 16), 1);
    }
}
