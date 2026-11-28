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
                    "{stage:?} {} bytes starting {:02x?}",
                    code.len(),
                    &code[..code.len().min(8)]
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
        }
    }
}
