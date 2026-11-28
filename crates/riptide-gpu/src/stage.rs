//! What a translated shader is bound to besides the heaps: the one host root signature,
//! and the blocks of memory each draw's stage inputs are written into (design T7).
//!
//! Every host pipeline has the same root signature:
//!
//! | Parameter | Is | Holds |
//! | --- | --- | --- |
//! | 0 | constants `b0`, space 0 | the vertex or mesh program's stage inputs |
//! | 1 | constants `b0`, space 1 | the pixel program's |
//! | 2 | constants `b0`, space 2 | the compute program's |
//! | 3 | a buffer of words `t0`, space 0 | the page table |
//!
//! and both heaps of `bindless.rs` indexed directly. A stage's inputs are a block of 80
//! words: 64 of user data, then 16 the stage is told besides. A block is written once,
//! when the draw is recorded, and read by the GPU whenever the list runs, which may be
//! many times; so blocks belong to the allocator the list records into and are given back
//! when the title resets or frees it, which it may only do when the GPU is done with
//! everything recorded there.

// Nothing draws with translated shaders yet; a test on a real GPU does (`hosttest.rs`).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::{
    D3D_ROOT_SIGNATURE_VERSION_1_1, D3D12_FEATURE_D3D12_OPTIONS16,
    D3D12_FEATURE_DATA_D3D12_OPTIONS16, D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_GPU_UPLOAD, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON,
    D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_ROOT_DESCRIPTOR_FLAG_NONE, D3D12_ROOT_DESCRIPTOR1,
    D3D12_ROOT_PARAMETER_TYPE, D3D12_ROOT_PARAMETER_TYPE_CBV, D3D12_ROOT_PARAMETER_TYPE_SRV,
    D3D12_ROOT_PARAMETER1, D3D12_ROOT_PARAMETER1_0, D3D12_ROOT_SIGNATURE_DESC1,
    D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
    D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
    D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED, D3D12_SHADER_VISIBILITY_ALL,
    D3D12_TEXTURE_LAYOUT_ROW_MAJOR, D3D12_VERSIONED_ROOT_SIGNATURE_DESC,
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0, D3D12SerializeVersionedRootSignature, ID3D12Resource,
    ID3D12RootSignature,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};

use crate::trace;

/// The root parameter of the vertex or mesh program's inputs. The pixel program's is the
/// next and the compute program's the one after.
pub(crate) const INPUTS: u32 = 0;
/// The root parameter of the page table.
pub(crate) const PAGE_TABLE: u32 = 3;

/// How many words a stage's inputs are: 64 of user data, then 16 more.
pub(crate) const WORDS: usize = 80;
/// The bytes a block takes: the host wants constants to start on 256 bytes.
const BLOCK: usize = 512;
/// How many blocks a chunk has. A chunk is one host buffer.
const BLOCKS: usize = 512;

/// The one host root signature, made the first time it is asked for.
pub(crate) fn root_signature() -> Option<ID3D12RootSignature> {
    use std::sync::OnceLock;
    /// The signature, which is free-threaded.
    struct Kept(Option<ID3D12RootSignature>);
    // SAFETY: host root signatures may be used from any thread.
    unsafe impl Send for Kept {}
    // SAFETY: as above.
    unsafe impl Sync for Kept {}
    static SIGNATURE: OnceLock<Kept> = OnceLock::new();
    SIGNATURE
        .get_or_init(|| {
            let made = make_root_signature();
            if made.is_none() {
                trace!("the host root signature of translated shaders could not be made");
            }
            Kept(made)
        })
        .0
        .clone()
}

fn make_root_signature() -> Option<ID3D12RootSignature> {
    let host = crate::host::host().ok()?;
    let view = |kind: D3D12_ROOT_PARAMETER_TYPE, space: u32| D3D12_ROOT_PARAMETER1 {
        ParameterType: kind,
        Anonymous: D3D12_ROOT_PARAMETER1_0 {
            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                ShaderRegister: 0,
                RegisterSpace: space,
                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_NONE,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let parameters = [
        view(D3D12_ROOT_PARAMETER_TYPE_CBV, 0),
        view(D3D12_ROOT_PARAMETER_TYPE_CBV, 1),
        view(D3D12_ROOT_PARAMETER_TYPE_CBV, 2),
        view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
    ];
    let description = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: parameters.len() as u32,
                pParameters: parameters.as_ptr(),
                NumStaticSamplers: 0,
                pStaticSamplers: std::ptr::null(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
                    | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            },
        },
    };
    let mut packed: Option<ID3DBlob> = None;
    // SAFETY: plain calls: the description and what it points at outlive the first, and
    // the packed signature is read inside its own size by the second.
    unsafe {
        D3D12SerializeVersionedRootSignature(&description, &mut packed, None).ok()?;
        let packed = packed?;
        let bytes = std::slice::from_raw_parts(
            packed.GetBufferPointer().cast::<u8>(),
            packed.GetBufferSize(),
        );
        host.device.CreateRootSignature(0, bytes).ok()
    }
}

/// One host buffer of blocks, which the CPU writes and the GPU reads.
struct Chunk {
    /// Kept so that the memory lives; the blocks are reached through the two below.
    _buffer: ID3D12Resource,
    cpu: *mut u8,
    gpu: u64,
    used: usize,
}

// SAFETY: the buffer is a free-threaded host object, and its mapping is only written
// under the lock the chunk is kept behind.
unsafe impl Send for Chunk {}

impl Chunk {
    fn new() -> Option<Chunk> {
        let host = crate::host::host().ok()?;
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
        // SAFETY: a plain query with room for its answer.
        let fast = unsafe {
            host.device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS16,
                    (&raw mut options).cast(),
                    size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS16>() as u32,
                )
                .is_ok()
        } && options.GPUUploadHeapSupported.as_bool();
        let description = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: (BLOCK * BLOCKS) as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        // Video memory the CPU writes where the machine has it, since the GPU reads a
        // block for every draw; ordinary upload memory otherwise.
        let (kind, state) = if fast {
            (D3D12_HEAP_TYPE_GPU_UPLOAD, D3D12_RESOURCE_STATE_COMMON)
        } else {
            (D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: kind,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain calls on the host device and the new buffer, which stays mapped
        // for as long as it lives.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &description,
                    state,
                    None,
                    &mut made,
                )
                .ok()?;
            let buffer = made?;
            let mut cpu = std::ptr::null_mut();
            buffer.Map(0, None, Some(&mut cpu)).ok()?;
            Some(Chunk {
                gpu: buffer.GetGPUVirtualAddress(),
                cpu: cpu.cast(),
                _buffer: buffer,
                used: 0,
            })
        }
    }
}

#[derive(Default)]
struct Chunks {
    /// The chunks each allocator holds, by the allocator; the last is the one being
    /// filled.
    held: HashMap<usize, Vec<Chunk>>,
    /// Chunks no allocator holds.
    spare: Vec<Chunk>,
}

static CHUNKS: Mutex<Option<Chunks>> = Mutex::new(None);

/// Writes a stage's inputs into a new block that lives as long as what `allocator` (any
/// number that stands for the title's allocator) holds, and returns where the host GPU
/// finds it. `None` if the host gave no memory.
pub(crate) fn block(allocator: usize, words: &[u32; WORDS]) -> Option<u64> {
    let mut chunks = CHUNKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let chunks = chunks.get_or_insert_with(Chunks::default);
    let full = chunks
        .held
        .get(&allocator)
        .and_then(|held| held.last())
        .is_none_or(|chunk| chunk.used == BLOCKS);
    if full {
        let mut chunk = match chunks.spare.pop() {
            Some(chunk) => chunk,
            None => Chunk::new()?,
        };
        chunk.used = 0;
        chunks.held.entry(allocator).or_default().push(chunk);
    }
    let chunk = chunks.held.get_mut(&allocator)?.last_mut()?;
    let at = chunk.used * BLOCK;
    chunk.used += 1;
    // SAFETY: the block is inside the chunk's mapping, and is written only here, under
    // the lock.
    unsafe {
        std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), chunk.cpu.add(at), WORDS * 4);
    }
    Some(chunk.gpu + at as u64)
}

/// Gives back every block written for `allocator`: the title has reset or freed it, so
/// the GPU is done with them.
pub(crate) fn give_back(allocator: usize) {
    let mut chunks = CHUNKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(chunks) = chunks.as_mut()
        && let Some(held) = chunks.held.remove(&allocator)
    {
        chunks.spare.extend(held);
    }
}

// A block holds a stage's inputs, and starts where the host wants constants to start.
const _: () = assert!(WORDS * 4 <= BLOCK && BLOCK % 256 == 0);
