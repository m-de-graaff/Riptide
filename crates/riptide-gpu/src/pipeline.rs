//! Pipeline states: a set of shader programs and the fixed state they draw with.
//!
//! A title's shader programs are the console GPU's machine code, which the host GPU cannot
//! run. They have to be translated first, and the translator (`riptide-shader`) is not
//! built yet. So a pipeline made here keeps what the title described, whole, and has no
//! host pipeline: what would be drawn with it is not drawn (decision D7 already skips the
//! draws of a pipeline that is not ready). The copy is what a host pipeline is made from
//! once programs can be translated.

use std::sync::OnceLock;

use crate::device::Device;
use crate::object::{self, Child, E_INVALIDARG, Hresult, Iid, Object, guid};
use crate::trace;

/// `ID3D12PipelineState`.
const IID: Iid = guid(
    0x765a30f3,
    0xf624,
    0x4c6f,
    [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
);

/// `D3D12_PIPELINE_STATE_STREAM_DESC`: a size and that many bytes of subobjects.
#[repr(C)]
pub(crate) struct StreamDescription {
    bytes: usize,
    stream: *const u8,
}

/// `D3D12_SHADER_BYTECODE`: a program's bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Bytecode {
    code: *const u8,
    bytes: usize,
}

/// The stage a program runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Vertex,
    Pixel,
    Domain,
    Hull,
    Geometry,
    Compute,
    Amplification,
    Mesh,
}

/// What a subobject of a pipeline stream is, by its type number: how many bytes it takes
/// with its type, padded to 8, and the stage it is a program for if it is one. `None` for
/// a type whose size is not known, after which the stream cannot be followed.
///
/// The numbers and sizes are PC's, except that the console numbers amplification and mesh
/// programs 23 and 24.
fn subobject(kind: u32) -> Option<(usize, Option<Stage>)> {
    let program = |stage| Some((24, Some(stage)));
    match kind {
        1 => program(Stage::Vertex),
        2 => program(Stage::Pixel),
        3 => program(Stage::Domain),
        4 => program(Stage::Hull),
        5 => program(Stage::Geometry),
        6 => program(Stage::Compute),
        23 => program(Stage::Amplification),
        24 => program(Stage::Mesh),
        // The root signature: a pointer.
        0 => Some((16, None)),
        // The input layout and a cached pipeline: a pointer and a size or count.
        12 | 19 => Some((24, None)),
        // Stream output; render target formats.
        7 | 15 => Some((40, None)),
        // Blending for eight targets.
        8 => Some((336, None)),
        // The rasterizer; depth and stencil; samples; newer depth and stencil; views.
        10 => Some((48, None)),
        11 => Some((56, None)),
        17 => Some((16, None)),
        21 => Some((64, None)),
        22 => Some((32, None)),
        // One word: sample mask, strip cut, topology, depth format, node mask, flags.
        9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)),
        _ => None,
    }
}

/// The largest program that is believed: a size above it is a description gone wrong.
const LARGEST_PROGRAM: usize = 64 << 20;

/// One subobject of a stream: its type and where its bytes are in the copy of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Subobject {
    pub(crate) kind: u32,
    pub(crate) offset: usize,
    pub(crate) bytes: usize,
}

/// A pipeline state as a title's code sees it: the object header, then eight words the
/// title's own code copies into a command list when it writes commands itself, which it
/// does not do here.
#[repr(C)]
pub(crate) struct Pipeline {
    object: Object,
    packet: [u32; 8],
    /// The stream as the title gave it. Pointers in it are the title's and may not
    /// outlive the call; what they pointed at is in `programs` and `root`.
    stream: Vec<u8>,
    subobjects: Vec<Subobject>,
    /// Each stage's program, copied.
    programs: Vec<(Stage, Vec<u8>)>,
    /// The root signature object named in the stream, or zero.
    root: usize,
}

const _: () = assert!(std::mem::offset_of!(Pipeline, packet) == 0x10);

// SAFETY: `Pipeline` is `#[repr(C)]` and starts with its header.
unsafe impl Child for Pipeline {
    const NAME: &'static str = "pipeline state";
    const IIDS: &'static [Iid] = &[IID];

    fn header(&self) -> &Object {
        &self.object
    }
}

impl Pipeline {
    /// Each stage's program.
    #[allow(dead_code)] // the first user is making the host pipeline
    pub(crate) fn programs(&self) -> &[(Stage, Vec<u8>)] {
        &self.programs
    }

    /// The bytes of the subobject of type `kind`, with its type word.
    #[allow(dead_code)] // the first user is making the host pipeline
    pub(crate) fn subobject(&self, kind: u32) -> Option<&[u8]> {
        self.subobjects
            .iter()
            .find(|one| one.kind == kind)
            .map(|one| &self.stream[one.offset..one.offset + one.bytes])
    }

    /// The root signature object the pipeline was made with, or zero.
    #[allow(dead_code)] // the first user is making the host pipeline
    pub(crate) fn root(&self) -> usize {
        self.root
    }
}

/// The pipeline has methods of its own from position 8, none of which a title has called.
const SLOTS: usize = 12;

const NAMES: [(usize, &str); 9] = [
    (0, "QueryInterface"),
    (1, "AddRef"),
    (2, "Release"),
    (3, "GetPrivateData"),
    (4, "SetPrivateData"),
    (5, "SetPrivateDataInterface"),
    (6, "SetName"),
    (7, "GetDevice"),
    (8, "GetCachedBlob"),
];

fn table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let methods = object::child_methods::<Pipeline>();
        object::table("ID3D12PipelineState", SLOTS, &NAMES, &methods)
    })
}

/// Follows a stream: every subobject up to the first whose size is not known, which is
/// returned as the second value.
fn follow(stream: &[u8]) -> (Vec<Subobject>, Option<(u32, usize)>) {
    let mut subobjects = Vec::new();
    let mut at = 0;
    while at + 8 <= stream.len() {
        let kind = u32::from_le_bytes([stream[at], stream[at + 1], stream[at + 2], stream[at + 3]]);
        let Some((bytes, _)) = subobject(kind) else {
            return (subobjects, Some((kind, at)));
        };
        if at + bytes > stream.len() {
            return (subobjects, Some((kind, at)));
        }
        subobjects.push(Subobject {
            kind,
            offset: at,
            bytes,
        });
        at += bytes;
    }
    (subobjects, None)
}

/// The pointer-sized value 8 bytes into a subobject.
fn pointer(stream: &[u8], subobject: &Subobject) -> usize {
    let at = subobject.offset + 8;
    usize::from_le_bytes(stream[at..at + 8].try_into().expect("8 bytes"))
}

/// `ID3D12Device2::CreatePipelineState`: `(stream description, interface id, pipeline
/// out)`.
///
/// # Safety
/// The title's pointers: each null or valid; the stream is as long as its description says,
/// and each program in it as long as its subobject says.
pub(crate) unsafe extern "system" fn create(
    _device: *const Device,
    description: *const StreamDescription,
    iid: *const Iid,
    out: *mut *const Pipeline,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    if description.stream.is_null() || description.bytes == 0 {
        return E_INVALIDARG;
    }
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    // SAFETY: the title's stream, of that many bytes.
    let stream =
        unsafe { std::slice::from_raw_parts(description.stream, description.bytes) }.to_vec();
    let (subobjects, lost) = follow(&stream);
    if let Some((kind, at)) = lost {
        trace!(
            "a pipeline stream of {} bytes has a subobject of type {kind} at {at}, whose \
             size is not known; what follows it is not read",
            stream.len()
        );
    }
    let mut programs = Vec::new();
    let mut root = 0;
    for one in &subobjects {
        match subobject(one.kind) {
            Some((_, Some(stage))) => {
                // SAFETY: a program subobject is its type, then a pointer and a size.
                let code = unsafe {
                    stream
                        .as_ptr()
                        .add(one.offset + 8)
                        .cast::<Bytecode>()
                        .read_unaligned()
                };
                if code.code.is_null() || code.bytes == 0 {
                    continue;
                }
                if code.bytes > LARGEST_PROGRAM {
                    trace!(
                        "a {stage:?} program of {} bytes is not believed and not kept",
                        code.bytes
                    );
                    continue;
                }
                // SAFETY: the title's program, of that many bytes.
                let bytes = unsafe { std::slice::from_raw_parts(code.code, code.bytes) }.to_vec();
                crate::census::count(&bytes);
                programs.push((stage, bytes));
            }
            _ if one.kind == 0 => root = pointer(&stream, one),
            _ => {}
        }
    }
    let pipeline = Box::new(Pipeline {
        object: Object::new(table(), 1),
        packet: [0; 8],
        stream,
        subobjects,
        programs,
        root,
    });
    show(&pipeline);
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(pipeline, out) }
}

/// `D3D12_COMPUTE_PIPELINE_STATE_DESC`, as on PC.
#[repr(C)]
pub(crate) struct ComputeDescription {
    root: usize,
    program: Bytecode,
    nodes: u32,
    cached: Bytecode,
    flags: u32,
}

const _: () = assert!(size_of::<ComputeDescription>() == 56);

/// `ID3D12Device::CreateComputePipelineState`: `(description, interface id, pipeline
/// out)`. The program and the root signature are kept, as [`create`] keeps a stream's.
///
/// # Safety
/// The title's pointers: each null or valid, and the program as long as the description
/// says.
pub(crate) unsafe extern "system" fn create_compute(
    _device: *const Device,
    description: *const ComputeDescription,
    iid: *const Iid,
    out: *mut *const Pipeline,
) -> Hresult {
    // SAFETY: the title's description; null is refused.
    let Some(description) = (unsafe { description.as_ref() }) else {
        return E_INVALIDARG;
    };
    let code = description.program;
    if code.code.is_null() || code.bytes == 0 || code.bytes > LARGEST_PROGRAM {
        return E_INVALIDARG;
    }
    // SAFETY: the title's id and out-pointer.
    if let Err(code) = unsafe { object::accepts(iid, out) } {
        return code;
    }
    // SAFETY: the title's program, of that many bytes.
    let program = unsafe { std::slice::from_raw_parts(code.code, code.bytes) }.to_vec();
    crate::census::count(&program);
    let pipeline = Box::new(Pipeline {
        object: Object::new(table(), 1),
        packet: [0; 8],
        stream: Vec::new(),
        subobjects: Vec::new(),
        programs: vec![(Stage::Compute, program)],
        root: description.root,
    });
    show(&pipeline);
    // SAFETY: `accepts` checked the out-pointer.
    unsafe { object::hand_out(pipeline, out) }
}

/// `ID3D12Device::CreateComputePipelineStateX`: `(description, extension count,
/// extensions, interface id, pipeline out)`. The extensions are settings for the console
/// GPU's own code and are not kept: what they mean is not known.
///
/// # Safety
/// As [`create_compute`].
pub(crate) unsafe extern "system" fn create_compute_x(
    device: *const Device,
    description: *const ComputeDescription,
    extensions: u32,
    _extended: *const u8,
    iid: *const Iid,
    out: *mut *const Pipeline,
) -> Hresult {
    if extensions != 0 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SHOWN: AtomicU32 = AtomicU32::new(0);
        if SHOWN.fetch_add(1, Ordering::Relaxed) == 0 {
            trace!("a compute pipeline has {extensions} console extensions, which are not kept");
        }
    }
    // SAFETY: as received.
    unsafe { create_compute(device, description, iid, out) }
}

/// Device positions 57 and 67: two of the console's own ways to make a pipeline, `(this,
/// description, interface id, pipeline out)`. The title asks both for a pipeline state.
/// What their descriptions hold is not known, so no pipeline can be kept from them: the
/// call fails with the out-pointer cleared, and the title goes on without that pipeline.
///
/// # Safety
/// `out` is null or where the pipeline would go.
unsafe fn refuse_unknown(
    slot: usize,
    description: *const u8,
    out: *mut *const Pipeline,
) -> Hresult {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CALLS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
    let calls = CALLS[usize::from(slot == 67)].fetch_add(1, Ordering::Relaxed) + 1;
    if calls == 1 {
        trace!(
            "device position {slot} (+{:#x}) makes a pipeline from a description that is not understood (at {description:p}); the title goes without it",
            slot * 8
        );
    }
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = std::ptr::null();
    }
    object::E_NOTIMPL
}

/// Device position 57. See [`refuse_unknown`].
///
/// # Safety
/// As [`refuse_unknown`].
pub(crate) unsafe extern "system" fn create_unknown_57(
    _device: *const Device,
    description: *const u8,
    _iid: *const Iid,
    out: *mut *const Pipeline,
) -> Hresult {
    // SAFETY: as received.
    unsafe { refuse_unknown(57, description, out) }
}

/// Device position 67. See [`refuse_unknown`].
///
/// # Safety
/// As [`refuse_unknown`].
pub(crate) unsafe extern "system" fn create_unknown_67(
    _device: *const Device,
    description: *const u8,
    _iid: *const Iid,
    out: *mut *const Pipeline,
) -> Hresult {
    // SAFETY: as received.
    unsafe { refuse_unknown(67, description, out) }
}

/// What a program's container holds, in a line: each part's four-letter name and size, and
/// for the `DXIL` part where its bitcode starts and how long it is. A program is a DXBC
/// container: a 32-byte header, a list of part offsets, and parts that each start with a
/// name and a size. `None` if the bytes are not one.
pub(crate) fn container_parts(code: &[u8]) -> Option<String> {
    let word = |at: usize| {
        code.get(at..at + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    };
    if code.get(..4)? != b"DXBC" {
        return None;
    }
    let count = word(28)?;
    let mut parts = Vec::new();
    for index in 0..count.min(64) {
        let at = word(32 + index * 4)?;
        let name = String::from_utf8_lossy(code.get(at..at + 4)?).into_owned();
        let bytes = word(at + 4)?;
        let data = at + 8;
        if name == "DXIL" || name == "STAT" {
            // The part starts with a program version and a size in words, then "DXIL", a
            // version, and where the bitcode starts (counted from that "DXIL") and its
            // length. On PC the bitcode starts 16 bytes in and runs to the end.
            let version = word(data)?;
            let start = word(data + 16)?;
            let length = word(data + 20)?;
            let magic = code.get(data + 8 + start..data + 8 + start + 4);
            parts.push(format!(
                "{name} {bytes} (program {version:#x}, bitcode {length} bytes from {start}{})",
                if magic == Some(&[0x42, 0x43, 0xc0, 0xde]) {
                    ""
                } else {
                    ", not bitcode there"
                }
            ));
        } else {
            parts.push(format!("{name} {bytes}"));
        }
    }
    Some(parts.join(", "))
}

/// Says what was made, for the first few: a title makes thousands.
fn show(pipeline: &Pipeline) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SHOWN: AtomicU32 = AtomicU32::new(0);
    let shown = SHOWN.fetch_add(1, Ordering::Relaxed);
    if shown == 0 {
        trace!(
            "pipelines are kept as described and not made on the host: shader programs \
             cannot be translated yet, so nothing is drawn with them"
        );
    }
    if shown < 6 {
        let programs: Vec<String> = pipeline
            .programs
            .iter()
            .map(|(stage, code)| {
                format!(
                    "{stage:?} {} bytes: {}",
                    code.len(),
                    container_parts(code).unwrap_or_else(|| format!(
                        "not a DXBC container, starting {:02x?}",
                        &code[..code.len().min(8)]
                    ))
                )
            })
            .collect();
        let kinds: Vec<u32> = pipeline.subobjects.iter().map(|one| one.kind).collect();
        trace!(
            "pipeline {:p} kept: subobjects {kinds:?}, root signature {:#x}, programs \
             {programs:?}",
            &raw const *pipeline, pipeline.root
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, method};

    /// A subobject: its type, padding to 8 bytes, then its payload.
    fn push(stream: &mut Vec<u8>, kind: u32, payload: &[u8]) {
        stream.extend(kind.to_le_bytes());
        stream.extend([0; 4]);
        stream.extend(payload);
    }

    #[test]
    fn a_containers_parts_are_named_with_their_sizes() {
        // A header, one part offset, and a part "ABCD" of 4 bytes.
        let mut code = b"DXBC".to_vec();
        code.extend([0; 24]);
        code.extend(1u32.to_le_bytes());
        code.extend(36u32.to_le_bytes());
        code.extend(b"ABCD");
        code.extend(4u32.to_le_bytes());
        code.extend([9; 4]);
        assert_eq!(container_parts(&code).as_deref(), Some("ABCD 4"));
        assert_eq!(container_parts(b"nope"), None);
        // A part offset that points outside the bytes is not followed.
        code[32] = 200;
        assert_eq!(container_parts(&code), None);
    }

    #[test]
    fn a_stream_is_followed_subobject_by_subobject() {
        let mut stream = Vec::new();
        push(&mut stream, 0, &0x1234usize.to_le_bytes());
        push(&mut stream, 14, &[]);
        push(&mut stream, 10, &[0; 40]);
        let (subobjects, lost) = follow(&stream);
        assert_eq!(lost, None);
        assert_eq!(
            subobjects,
            [
                Subobject {
                    kind: 0,
                    offset: 0,
                    bytes: 16
                },
                Subobject {
                    kind: 14,
                    offset: 16,
                    bytes: 8
                },
                Subobject {
                    kind: 10,
                    offset: 24,
                    bytes: 48
                },
            ]
        );
        assert_eq!(pointer(&stream, &subobjects[0]), 0x1234);

        // An unknown type, or a subobject cut short, ends the reading there.
        push(&mut stream, 77, &[0; 8]);
        assert_eq!(follow(&stream).1, Some((77, 72)));
        let mut short = Vec::new();
        push(&mut short, 8, &[0; 16]);
        assert_eq!(follow(&short), (Vec::new(), Some((8, 0))));
    }

    #[test]
    fn a_pipeline_keeps_its_programs_and_root_signature() {
        let vertex = [0xaau8; 40];
        let pixel = [0xbbu8; 24];
        let mut stream = Vec::new();
        push(&mut stream, 0, &0x7000usize.to_le_bytes());
        for (kind, code) in [(1u32, &vertex[..]), (2, &pixel[..])] {
            let mut payload = (code.as_ptr() as usize).to_le_bytes().to_vec();
            payload.extend(code.len().to_le_bytes());
            push(&mut stream, kind, &payload);
        }
        // A stage with no program is left out.
        push(&mut stream, 5, &[0; 16]);
        push(&mut stream, 16, &[]);
        let description = StreamDescription {
            bytes: stream.len(),
            stream: stream.as_ptr(),
        };
        let mut made: *const Pipeline = std::ptr::null();
        let device = std::ptr::null();
        // SAFETY: a valid stream whose programs are the arrays above; the table has a
        // release method at position 2.
        unsafe {
            assert_eq!(
                create(device, &description, &[9u8; 16], &mut made),
                E_NOINTERFACE
            );
            assert_eq!(create(device, &description, &IID, &mut made), 0);
            let pipeline = &*made;
            assert_eq!(pipeline.root(), 0x7000);
            assert_eq!(
                pipeline.programs(),
                [
                    (Stage::Vertex, vertex.to_vec()),
                    (Stage::Pixel, pixel.to_vec())
                ]
            );
            assert_eq!(pipeline.subobject(16).map(<[u8]>::len), Some(8));
            assert_eq!(pipeline.subobject(8), None);
            // The words a title's own code would copy are zero.
            let words = made.cast::<u8>().add(0x10).cast::<[u32; 8]>().read();
            assert_eq!(words, [0; 8]);
            let release: unsafe extern "system" fn(*const Pipeline) -> u32 = method(made.cast(), 2);
            assert_eq!(release(made), 0);

            assert_eq!(
                create(device, std::ptr::null(), &IID, &mut made),
                E_INVALIDARG
            );

            // A compute pipeline keeps its one program.
            let description = ComputeDescription {
                root: 0x9000,
                program: Bytecode {
                    code: vertex.as_ptr(),
                    bytes: vertex.len(),
                },
                nodes: 0,
                cached: Bytecode {
                    code: std::ptr::null(),
                    bytes: 0,
                },
                flags: 0,
            };
            assert_eq!(
                create_compute_x(device, &description, 0, std::ptr::null(), &IID, &mut made),
                0
            );
            assert_eq!((*made).root(), 0x9000);
            assert_eq!((*made).programs(), [(Stage::Compute, vertex.to_vec())]);
            assert_eq!(release(made), 0);
            let empty = ComputeDescription {
                program: description.cached,
                ..description
            };
            assert_eq!(
                create_compute(device, &empty, &IID, &mut made),
                E_INVALIDARG
            );

            // The two creation methods that are not understood fail and clear the pointer.
            made = std::ptr::dangling();
            assert!(create_unknown_67(device, std::ptr::null(), &IID, &mut made) < 0);
            assert!(made.is_null());
            assert!(create_unknown_57(device, std::ptr::null(), &IID, std::ptr::null_mut()) < 0);
        }
    }
}
