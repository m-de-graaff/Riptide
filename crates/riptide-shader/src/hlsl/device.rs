//! Emitted shaders run on a real D3D12 device and compared with the
//! reference interpreter.
//!
//! A test here builds a compute function by hand, emits its HLSL, compiles
//! it, and runs it on the software adapter (WARP, which every Windows has)
//! against a small title memory set up exactly as the binding contract says:
//! a page table at `t0`, the bank of memory at places 0 and 16 of the
//! resource heap, and the stage inputs at `b0` in space 2. The same function
//! runs in [`crate::interp::run_wave`] against a copy of that memory, and
//! the two memories are compared afterwards, word for word.
//!
//! Each lane (one thread of a group of 64) reads its operands from its own
//! place in memory and writes its results to its own place, so one run
//! checks an operation on thousands of operand values.
//!
//! The tests need the shader compiler and a D3D12 device, so they are run
//! by name: `cargo test -p riptide-shader -- --ignored on_a_device`. Where
//! either is missing they say so and pass.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::{Mutex, MutexGuard};

use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIFactory4,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface as _;

use crate::container::xbox::HardwareStage;
use crate::interp::{Host, run_wave};
use crate::ir::cfg::make_reducible;
use crate::ir::eval::{self, BufferFormat};
use crate::ir::{
    Address, AtomicOp, AtomicPlace, BinaryOp, BufferPosition, CompareOp, ExportTarget, FlagOp,
    Function, ImageOp, Input, Interface, Op, Terminator, TernaryOp, UnaryOp, Value, Var,
};

use super::testing::{Maker, compile};
use super::{Options, Signatures, emit};

/// The size of a page of title memory.
const PAGE: usize = 1 << 16;

/// A small title memory: a few pages, each at a title address of its own.
/// A page's frame in the one bank the tests have is its index here.
#[derive(Clone, PartialEq, Eq)]
struct Memory {
    pages: Vec<(u64, Vec<u8>)>,
}

impl Memory {
    /// Memory with the given pages (title address divided by the page
    /// size), all zero.
    fn new(pages: &[u64]) -> Memory {
        Memory {
            pages: pages.iter().map(|&page| (page, vec![0; PAGE])).collect(),
        }
    }

    fn byte(&mut self, address: u64) -> Option<&mut u8> {
        let page = address >> 16;
        self.pages
            .iter_mut()
            .find(|(number, _)| *number == page)
            .map(|(_, bytes)| &mut bytes[(address & 0xFFFF) as usize])
    }

    fn load(&self, address: u64, width: u8) -> u32 {
        (0..u64::from(width / 8))
            .map(|index| {
                let at = address.wrapping_add(index);
                let byte = self
                    .pages
                    .iter()
                    .find(|(number, _)| *number == at >> 16)
                    .map_or(0, |(_, bytes)| bytes[(at & 0xFFFF) as usize]);
                u32::from(byte) << (8 * index)
            })
            .sum()
    }

    fn store(&mut self, address: u64, width: u8, value: u32) {
        for index in 0..u64::from(width / 8) {
            if let Some(byte) = self.byte(address.wrapping_add(index)) {
                *byte = (value >> (8 * index)) as u8;
            }
        }
    }

    fn word(&self, address: u64) -> u32 {
        self.load(address, 32)
    }

    fn set_word(&mut self, address: u64, value: u32) {
        self.store(address, 32, value);
    }
}

/// The interpreter's view of a run: the memory, the stage inputs, and which
/// group of threads is running.
struct Reference<'a> {
    memory: &'a mut Memory,
    words: &'a [u32; 80],
    group: u32,
}

impl Host for Reference<'_> {
    fn input(&mut self, lane: usize, input: Input) -> u32 {
        match input {
            Input::UserData(n) => self.words[usize::from(n)],
            Input::ThreadId(0) => lane as u32,
            Input::GroupId(0) => self.group,
            _ => 0,
        }
    }

    fn load(&mut self, address: u64, width: u8) -> u32 {
        // The page table covers addresses below 2^37.
        if address >> 37 != 0 {
            return 0;
        }
        self.memory.load(address, width)
    }

    fn store(&mut self, address: u64, width: u8, value: u32) {
        if address >> 37 == 0 {
            self.memory.store(address, width, value);
        }
    }

    fn image(&mut self, _lane: usize, _access: &ImageOp) -> [u32; 4] {
        [0; 4]
    }

    fn export(&mut self, _lane: usize, _target: ExportTarget, _values: [Option<u32>; 4]) {}

    fn discard(&mut self, _lane: usize) {}
}

/// A D3D12 device with what a translated compute shader is bound to.
struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    root: ID3D12RootSignature,
    heap: ID3D12DescriptorHeap,
    /// The page table, which the CPU writes and the GPU reads.
    page_table: ID3D12Resource,
    page_table_at: *mut u32,
    /// The block of stage inputs.
    inputs: ID3D12Resource,
    inputs_at: *mut u32,
    /// The bank of title memory, with the buffers that fill it and read it
    /// back.
    bank: ID3D12Resource,
    upload: ID3D12Resource,
    upload_at: *mut u8,
    read_back: ID3D12Resource,
    bank_bytes: usize,
    /// Held for as long as the test uses the device.
    _turn: MutexGuard<'static, ()>,
}

/// How many entries the page table has: one per page below 2^37.
const TABLE_ENTRIES: usize = 1 << 21;

impl Gpu {
    /// Makes the device and its buffers, with a bank of `pages` pages.
    /// `None`, with the reason printed, where the machine has no device
    /// that runs shader model 6.6.
    fn new(pages: usize) -> Option<Gpu> {
        // One test at a time: a process has one device for the adapter,
        // and the software adapter does not take several tests at once.
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let turn = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bank_bytes = pages * PAGE;
        // SAFETY: plain calls on the factory, the device and objects made
        // here, each with descriptions that outlive the call. The mapped
        // pointers belong to buffers the struct keeps alive.
        unsafe {
            let made = (|| -> windows::core::Result<Gpu> {
                let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
                let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
                let device = device.ok_or_else(windows::core::Error::empty)?;
                let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                    HighestShaderModel: D3D_SHADER_MODEL_6_6,
                };
                device.CheckFeatureSupport(
                    D3D12_FEATURE_SHADER_MODEL,
                    (&raw mut model).cast::<c_void>(),
                    size_of::<D3D12_FEATURE_DATA_SHADER_MODEL>() as u32,
                )?;
                if model.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0 {
                    return Err(windows::core::Error::empty());
                }
                let queue: ID3D12CommandQueue =
                    device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())?;

                // The root signature of the contract, as far as a compute
                // shader sees it: its inputs, the page table, and the
                // resource heap indexed directly.
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
                            Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
                        },
                    },
                };
                let mut packed: Option<ID3DBlob> = None;
                D3D12SerializeVersionedRootSignature(&description, &mut packed, None)?;
                let packed = packed.ok_or_else(windows::core::Error::empty)?;
                let root: ID3D12RootSignature = device.CreateRootSignature(
                    0,
                    std::slice::from_raw_parts(
                        packed.GetBufferPointer().cast::<u8>(),
                        packed.GetBufferSize(),
                    ),
                )?;

                let buffer = |bytes: usize,
                              kind: D3D12_HEAP_TYPE,
                              flags: D3D12_RESOURCE_FLAGS,
                              state: D3D12_RESOURCE_STATES|
                 -> windows::core::Result<ID3D12Resource> {
                    let mut made: Option<ID3D12Resource> = None;
                    device.CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: kind,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                            Width: bytes as u64,
                            Height: 1,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT_UNKNOWN,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                            Flags: flags,
                            ..Default::default()
                        },
                        state,
                        None,
                        &mut made,
                    )?;
                    made.ok_or_else(windows::core::Error::empty)
                };
                let mapped = |resource: &ID3D12Resource| -> windows::core::Result<*mut c_void> {
                    let mut at = std::ptr::null_mut();
                    resource.Map(0, None, Some(&mut at))?;
                    Ok(at)
                };
                let page_table = buffer(
                    TABLE_ENTRIES * 4,
                    D3D12_HEAP_TYPE_UPLOAD,
                    D3D12_RESOURCE_FLAG_NONE,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                )?;
                let inputs = buffer(
                    512,
                    D3D12_HEAP_TYPE_UPLOAD,
                    D3D12_RESOURCE_FLAG_NONE,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                )?;
                let upload = buffer(
                    bank_bytes,
                    D3D12_HEAP_TYPE_UPLOAD,
                    D3D12_RESOURCE_FLAG_NONE,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                )?;
                let read_back = buffer(
                    bank_bytes,
                    D3D12_HEAP_TYPE_READBACK,
                    D3D12_RESOURCE_FLAG_NONE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )?;
                let bank = buffer(
                    bank_bytes,
                    D3D12_HEAP_TYPE_DEFAULT,
                    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COMMON,
                )?;

                // The bank at place 0 to read and at place 16 to write.
                let heap: ID3D12DescriptorHeap =
                    device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: 64,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })?;
                let step = device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                    as usize;
                let start = heap.GetCPUDescriptorHandleForHeapStart();
                let place = |index: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start.ptr + index * step,
                };
                let words = (bank_bytes / 4) as u32;
                device.CreateShaderResourceView(
                    &bank,
                    Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_TYPELESS,
                        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_SRV {
                                FirstElement: 0,
                                NumElements: words,
                                StructureByteStride: 0,
                                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                            },
                        },
                    }),
                    place(0),
                );
                device.CreateUnorderedAccessView(
                    &bank,
                    None,
                    Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_TYPELESS,
                        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_UAV {
                                FirstElement: 0,
                                NumElements: words,
                                StructureByteStride: 0,
                                CounterOffsetInBytes: 0,
                                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                            },
                        },
                    }),
                    place(16),
                );
                Ok(Gpu {
                    _turn: turn,
                    page_table_at: mapped(&page_table)?.cast(),
                    inputs_at: mapped(&inputs)?.cast(),
                    upload_at: mapped(&upload)?.cast(),
                    device,
                    queue,
                    root,
                    heap,
                    page_table,
                    inputs,
                    bank,
                    upload,
                    read_back,
                    bank_bytes,
                })
            })();
            match made {
                Ok(gpu) => Some(gpu),
                Err(error) => {
                    eprintln!("no device that runs shader model 6.6 ({error}): nothing is checked");
                    None
                }
            }
        }
    }

    /// Runs a compiled compute shader for `groups` groups of threads with
    /// the stage inputs `words` against `memory`, which holds what the
    /// shader left when this returns.
    fn run(&self, shader: &[u8], words: &[u32; 80], memory: &mut Memory, groups: u32) {
        assert!(memory.pages.len() * PAGE <= self.bank_bytes);
        // SAFETY: the mapped pointers are written inside their buffers'
        // sizes; the calls are plain calls on objects made here, in the
        // order the host wants; the read-back buffer is read after the GPU
        // is done.
        unsafe {
            // The page table names the frame of each page, plus one.
            for (frame, (page, bytes)) in memory.pages.iter().enumerate() {
                assert!((*page as usize) < TABLE_ENTRIES);
                self.page_table_at
                    .add(*page as usize)
                    .write(frame as u32 + 1);
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.upload_at.add(frame * PAGE),
                    PAGE,
                );
            }
            std::ptr::copy_nonoverlapping(words.as_ptr(), self.inputs_at, words.len());

            let mut description = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: ManuallyDrop::new(Some(self.root.clone())),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: shader.as_ptr().cast(),
                    BytecodeLength: shader.len(),
                },
                ..Default::default()
            };
            let pipeline: windows::core::Result<ID3D12PipelineState> =
                self.device.CreateComputePipelineState(&description);
            ManuallyDrop::drop(&mut description.pRootSignature);
            let pipeline = pipeline.expect("the host takes the compiled shader");

            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = self
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .unwrap();
            let bytes = (memory.pages.len() * PAGE) as u64;
            let transition = |before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES| {
                D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: ManuallyDrop::new(Some(self.bank.clone())),
                            Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                            StateBefore: before,
                            StateAfter: after,
                        }),
                    },
                }
            };
            list.CopyBufferRegion(&self.bank, 0, &self.upload, 0, bytes);
            list.ResourceBarrier(&[transition(
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.SetDescriptorHeaps(&[Some(self.heap.clone())]);
            list.SetComputeRootSignature(&self.root);
            list.SetComputeRootConstantBufferView(0, self.inputs.GetGPUVirtualAddress());
            list.SetComputeRootShaderResourceView(1, self.page_table.GetGPUVirtualAddress());
            list.SetPipelineState(&pipeline);
            list.Dispatch(groups, 1, 1);
            list.ResourceBarrier(&[transition(
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.CopyBufferRegion(&self.read_back, 0, &self.bank, 0, bytes);
            list.ResourceBarrier(&[transition(
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
            list.Close().unwrap();
            self.queue
                .ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            self.queue.Signal(&fence, 1).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            fence.SetEventOnCompletion(1, event).unwrap();
            WaitForSingleObject(event, INFINITE);
            let _ = windows::Win32::Foundation::CloseHandle(event);
            self.device.GetDeviceRemovedReason().unwrap();

            let mut at = std::ptr::null_mut();
            self.read_back.Map(0, None, Some(&mut at)).unwrap();
            for (frame, (page, bytes)) in memory.pages.iter_mut().enumerate() {
                std::ptr::copy_nonoverlapping(
                    at.cast::<u8>().add(frame * PAGE),
                    bytes.as_mut_ptr(),
                    PAGE,
                );
                // The next run starts from an empty table.
                self.page_table_at.add(*page as usize).write(0);
            }
            self.read_back.Unmap(0, None);
        }
    }
}

/// Runs `function` on the interpreter and on the device, each against a
/// copy of `memory`, and checks that the two memories agree in every word:
/// `same(address, expected, got)` decides, so that a test can allow the
/// host's rounding where the README says an operation is approximate.
/// Returns false where there is no compiler or device to check with.
fn check(
    gpu: Option<&Gpu>,
    function: &Function,
    memory: &Memory,
    words: &[u32; 80],
    groups: u32,
    same: impl Fn(u64, u32, u32) -> bool,
) -> bool {
    let emitted = emit(function, Signatures::default(), &Options::default()).unwrap();
    let Some(compiled) = compile(&emitted.source, "cs_6_6") else {
        eprintln!("no shader compiler: nothing is checked");
        return false;
    };
    let compiled = compiled.unwrap_or_else(|said| panic!("{said}\n{}", emitted.source));
    let Some(gpu) = gpu else {
        return false;
    };
    let mut expected = memory.clone();
    for group in 0..groups {
        let mut host = Reference {
            memory: &mut expected,
            words,
            group,
        };
        run_wave(function, 64, &mut host).unwrap();
    }
    let mut got = memory.clone();
    gpu.run(&compiled, words, &mut got, groups);
    let mut wrong = Vec::new();
    for ((page, expected), (_, got)) in expected.pages.iter().zip(&got.pages) {
        for index in (0..PAGE).step_by(4) {
            let read = |bytes: &[u8]| {
                u32::from_le_bytes([
                    bytes[index],
                    bytes[index + 1],
                    bytes[index + 2],
                    bytes[index + 3],
                ])
            };
            let address = (page << 16) + index as u64;
            if !same(address, read(expected), read(got)) {
                wrong.push((address, read(expected), read(got)));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} words differ; the first (address, interpreter, device): {:x?}",
        wrong.len(),
        &wrong[..wrong.len().min(24)]
    );
    true
}

/// A scrambling of a number.
fn mix(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Words that need care: both zeroes, ones, infinities, NaNs of both
/// kinds, the largest and smallest floats, the edges of the integer
/// conversions, quarter turns, and integers at the edges of their ranges.
const CAREFUL: [u32; 40] = [
    0x0000_0000,
    0x8000_0000,
    0x3F80_0000,
    0xBF80_0000,
    0x7F80_0000,
    0xFF80_0000,
    0x7FC0_0000,
    0x7F80_0001,
    0xFFC1_2345,
    0x7F7F_FFFF,
    0xFF7F_FFFF,
    0x0080_0000,
    0x8080_0000,
    0x3F00_0000,
    0x3E80_0000,
    0x3F40_0000,
    0xBE80_0000,
    0x4000_0000,
    0x4F00_0000,
    0xCF00_0000,
    0x4F80_0000,
    0x4EFF_FFFF,
    0x4380_0000,
    0xC380_0000,
    0x4380_0001,
    0x3F7F_FFFF,
    0x3F80_0001,
    0xB2DB_E6FF,
    0x477F_E000,
    0x477F_F000,
    0x3380_0000,
    0x3300_0000,
    0x0000_0001,
    0xFFFF_FFFF,
    0x7FFF_FFFF,
    0x0000_FFFF,
    0x0001_0000,
    0x0000_001F,
    0x0000_0020,
    0x1234_5678,
];

/// An operand for a lane: by turns a word that needs care, a float of
/// moderate size, a small integer and plain bits.
fn operand(lane: u32, which: u32) -> u32 {
    let hash = mix(u64::from(lane) << 8 | u64::from(which));
    match (lane + which) % 4 {
        0 => CAREFUL[(hash % CAREFUL.len() as u64) as usize],
        1 => ((hash % 65_536) as f32 / 256.0 - 128.0).to_bits(),
        2 => (hash % 100) as u32,
        _ => hash as u32,
    }
}

/// Whether a word is a NaN as a float.
fn is_nan(value: u32) -> bool {
    value & 0x7FFF_FFFF > 0x7F80_0000
}

/// Whether a word is a denormal float.
fn is_denormal(value: u32) -> bool {
    value & 0x7F80_0000 == 0 && value & 0x007F_FFFF != 0
}

/// How a result of the device is compared with the interpreter's.
#[derive(Clone, Copy, PartialEq)]
enum Check {
    /// The same bits.
    Exact,
    /// The same bits, or both a NaN: the host does not say which NaN an
    /// operation gives.
    Float,
    /// As `Float`, or within a few units of the last place, or within a
    /// hundred-thousandth: where the README says the host's own function is
    /// taken.
    Close,
}

impl Check {
    fn same(self, expected: u32, got: u32) -> bool {
        if expected == got {
            return true;
        }
        if self == Check::Exact {
            return false;
        }
        if is_nan(expected) && is_nan(got) {
            return true;
        }
        if self == Check::Float {
            return false;
        }
        let (a, b) = (f32::from_bits(expected), f32::from_bits(got));
        if a.is_nan() || b.is_nan() || a.is_infinite() || b.is_infinite() {
            return false;
        }
        // The software adapter's sine is good to a few millionths.
        let ulps = (i64::from(expected as i32) - i64::from(got as i32)).abs();
        (a - b).abs() < 1e-5 || (expected >> 31 == got >> 31 && ulps <= 16)
    }
}

const ALL_UNARY: [UnaryOp; 28] = [
    UnaryOp::Not,
    UnaryOp::CountBits,
    UnaryOp::FindLowBit,
    UnaryOp::LeadingZeros,
    UnaryOp::LeadingSignBits,
    UnaryOp::ReverseBits,
    UnaryOp::Saturate,
    UnaryOp::Reciprocal,
    UnaryOp::ReciprocalSqrt,
    UnaryOp::Sqrt,
    UnaryOp::Exp2,
    UnaryOp::Log2,
    UnaryOp::SinTurns,
    UnaryOp::CosTurns,
    UnaryOp::Floor,
    UnaryOp::Ceil,
    UnaryOp::Trunc,
    UnaryOp::RoundEven,
    UnaryOp::Fract,
    UnaryOp::FloatFromSigned,
    UnaryOp::FloatFromUnsigned,
    UnaryOp::SignedFromFloat,
    UnaryOp::UnsignedFromFloat,
    UnaryOp::HalfFromFloat,
    UnaryOp::HalfFromFloatTowardZero,
    UnaryOp::FloatFromHalf,
    // Twice more with other operands, further down.
    UnaryOp::Not,
    UnaryOp::Not,
];

const ALL_BINARY: [BinaryOp; 22] = [
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::Mul,
    BinaryOp::MulHighUnsigned,
    BinaryOp::MulHighSigned,
    BinaryOp::And,
    BinaryOp::Or,
    BinaryOp::Xor,
    BinaryOp::ShiftLeft,
    BinaryOp::ShiftRight,
    BinaryOp::ShiftRightSigned,
    BinaryOp::MinUnsigned,
    BinaryOp::MaxUnsigned,
    BinaryOp::MinSigned,
    BinaryOp::MaxSigned,
    BinaryOp::FloatAdd,
    BinaryOp::FloatSub,
    BinaryOp::FloatMul,
    BinaryOp::FloatMulLegacy,
    BinaryOp::FloatMin,
    BinaryOp::FloatMax,
    BinaryOp::FloatScale,
];

const ALL_TERNARY: [TernaryOp; 8] = [
    TernaryOp::FloatFused,
    TernaryOp::FloatMulAdd,
    TernaryOp::FloatMedian,
    TernaryOp::PermuteBytes,
    TernaryOp::CubeFace,
    TernaryOp::CubeS,
    TernaryOp::CubeT,
    TernaryOp::CubeMajor,
];

const ALL_COMPARE: [CompareOp; 24] = [
    CompareOp::Equal,
    CompareOp::NotEqual,
    CompareOp::UnsignedLess,
    CompareOp::UnsignedLessEqual,
    CompareOp::UnsignedGreater,
    CompareOp::UnsignedGreaterEqual,
    CompareOp::SignedLess,
    CompareOp::SignedLessEqual,
    CompareOp::SignedGreater,
    CompareOp::SignedGreaterEqual,
    CompareOp::FloatEqual,
    CompareOp::FloatLess,
    CompareOp::FloatLessEqual,
    CompareOp::FloatGreater,
    CompareOp::FloatGreaterEqual,
    CompareOp::FloatLessOrGreater,
    CompareOp::FloatOrdered,
    CompareOp::FloatUnordered,
    CompareOp::FloatNotEqual,
    CompareOp::FloatNotLess,
    CompareOp::FloatNotLessEqual,
    CompareOp::FloatNotGreater,
    CompareOp::FloatNotGreaterEqual,
    CompareOp::FloatNotLessOrGreater,
];

/// Where the operands of the lanes lie: above 4 GiB, so that both halves
/// of an address count.
const OPERANDS: u64 = 0x1_2340_0000;
/// Where the lanes write their results.
const RESULTS: u64 = 0x0080_0000;

/// The start of a test function: the lane's number among all groups, and
/// the address of its operands, whose two halves come from the stage
/// inputs.
fn lane_start(m: &mut Maker, operand_bytes: u32) -> (Value, Address) {
    let thread = m.op(Op::Input(Input::ThreadId(0)));
    let group = m.op(Op::Input(Input::GroupId(0)));
    let first = m.op(Op::Binary(BinaryOp::ShiftLeft, group, Value::Word(6)));
    let lane = m.op(Op::Binary(BinaryOp::Add, first, thread));
    let offset = m.op(Op::Binary(BinaryOp::Mul, lane, Value::Word(operand_bytes)));
    let base = m.op(Op::Input(Input::UserData(4)));
    let high = m.op(Op::Input(Input::UserData(5)));
    let low = m.op(Op::Binary(BinaryOp::Add, base, offset));
    (lane, Address { low, high })
}

/// The address of a lane's results, `bytes` for each lane.
fn results_at(m: &mut Maker, lane: Value, bytes: u32) -> Address {
    let offset = m.op(Op::Binary(BinaryOp::Mul, lane, Value::Word(bytes)));
    let low = m.op(Op::Binary(
        BinaryOp::Add,
        offset,
        Value::Word(RESULTS as u32),
    ));
    Address {
        low,
        high: Value::Word(0),
    }
}

/// The stage inputs of every test: the operands' address in words 4 and 5.
fn inputs() -> [u32; 80] {
    let mut words = [0; 80];
    words[4] = OPERANDS as u32;
    words[5] = (OPERANDS >> 32) as u32;
    words
}

/// The interface of every test function: groups of 64 threads in a row, a
/// lane to a thread.
fn groups_of_64() -> Interface {
    Interface {
        group_size: [64, 1, 1],
        ..Interface::default()
    }
}

/// A flag as a word.
fn as_word(m: &mut Maker, flag: Value) -> Value {
    m.op(Op::Select {
        condition: flag,
        if_true: Value::Word(1),
        if_false: Value::Word(0),
    })
}

#[test]
#[ignore = "needs the shader compiler and a D3D12 device"]
fn on_a_device_every_arithmetic_operation_gives_what_the_interpreter_gives() {
    const GROUPS: u32 = 48;
    let mut m = Maker::new();
    let (lane, operands) = lane_start(&mut m, 12);
    let loaded = m.all(Op::LoadMemory {
        address: operands,
        width: 32,
        words: 3,
    });
    let (a, b, c) = (loaded[0], loaded[1], loaded[2]);
    let mut results = Vec::new();
    let mut checks = Vec::new();
    for (index, op) in ALL_UNARY.into_iter().enumerate() {
        // The last two entries run the complement on the other operands,
        // which only fills the row.
        let operand = [a, b, c][if index < 26 { 0 } else { index - 25 }];
        results.push(m.op(Op::Unary(op, operand)));
        checks.push(match op {
            UnaryOp::Reciprocal
            | UnaryOp::ReciprocalSqrt
            | UnaryOp::Sqrt
            | UnaryOp::Exp2
            | UnaryOp::Log2
            | UnaryOp::SinTurns
            | UnaryOp::CosTurns => Check::Close,
            UnaryOp::Floor
            | UnaryOp::Ceil
            | UnaryOp::Trunc
            | UnaryOp::RoundEven
            | UnaryOp::Fract
            | UnaryOp::Saturate => Check::Float,
            _ => Check::Exact,
        });
    }
    for op in ALL_BINARY {
        results.push(m.op(Op::Binary(op, a, b)));
        checks.push(match op {
            BinaryOp::FloatAdd | BinaryOp::FloatSub | BinaryOp::FloatMul => Check::Float,
            BinaryOp::FloatMulLegacy | BinaryOp::FloatScale => Check::Float,
            _ => Check::Exact,
        });
    }
    // Shifts by a constant take another path through the emitter.
    for op in [
        BinaryOp::ShiftLeft,
        BinaryOp::ShiftRight,
        BinaryOp::ShiftRightSigned,
    ] {
        results.push(m.op(Op::Binary(op, a, Value::Word(37))));
        checks.push(Check::Exact);
    }
    let fused = results.len();
    for op in ALL_TERNARY {
        results.push(m.op(Op::Ternary(op, a, b, c)));
        checks.push(match op {
            TernaryOp::FloatFused => Check::Close,
            TernaryOp::PermuteBytes => Check::Exact,
            _ => Check::Float,
        });
    }
    for op in ALL_COMPARE {
        let flag = m.op(Op::Compare(op, a, b));
        results.push(as_word(&mut m, flag));
        checks.push(Check::Exact);
    }
    let class = m.op(Op::FloatClass {
        value: a,
        classes: b,
    });
    let odd = m.op(Op::Compare(CompareOp::UnsignedLess, b, c));
    let carry = m.op(Op::Carry {
        a,
        b,
        carry_in: odd,
    });
    let borrow = m.op(Op::Borrow {
        a,
        b,
        borrow_in: odd,
    });
    let not = m.op(Op::FlagNot(class));
    let and = m.op(Op::Flag(FlagOp::And, carry, borrow));
    let or = m.op(Op::Flag(FlagOp::Or, carry, not));
    let xor = m.op(Op::Flag(FlagOp::Xor, carry, borrow));
    let chosen = m.op(Op::Select {
        condition: xor,
        if_true: carry,
        if_false: class,
    });
    for flag in [class, carry, borrow, not, and, or, xor, chosen] {
        results.push(as_word(&mut m, flag));
        checks.push(Check::Exact);
    }
    let bytes = results.len() as u32 * 4;
    let address = results_at(&mut m, lane, bytes);
    m.op(Op::StoreMemory {
        address,
        width: 32,
        values: results,
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Compute, groups_of_64());

    let lanes = GROUPS * 64;
    let result_pages = (lanes * bytes).div_ceil(PAGE as u32) as u64;
    let mut pages = vec![OPERANDS >> 16];
    pages.extend((0..result_pages).map(|page| (RESULTS >> 16) + page));
    let mut memory = Memory::new(&pages);
    for lane in 0..lanes {
        for which in 0..3 {
            // The first lanes take every pair of the words that need care.
            let value = if (lane as usize) < CAREFUL.len() * CAREFUL.len() && which < 2 {
                let pair = lane as usize;
                CAREFUL[if which == 0 {
                    pair / CAREFUL.len()
                } else {
                    pair % CAREFUL.len()
                }]
            } else {
                operand(lane, which)
            };
            memory.set_word(OPERANDS + u64::from(lane * 12 + which * 4), value);
        }
    }
    let gpu = Gpu::new(pages.len());
    let operands = memory.clone();
    check(
        gpu.as_ref(),
        &function,
        &memory,
        &inputs(),
        GROUPS,
        |address, expected, got| {
            if !(RESULTS..OPERANDS).contains(&address) {
                return expected == got;
            }
            let offset = (address - RESULTS) as u32;
            let (lane, index) = (offset / bytes, (offset % bytes) / 4);
            let check = checks[index as usize];
            if check.same(expected, got) {
                return true;
            }
            let at = OPERANDS + u64::from(lane * 12);
            let [a, b, c] = [0, 4, 8].map(|which| operands.word(at + which));
            // The host decides whether a fused product is rounded before
            // the sum: either answer is its to give.
            if index as usize == fused
                && Check::Float.same(eval::ternary(TernaryOp::FloatMulAdd, a, b, c), got)
            {
                return true;
            }
            // The host's own functions (the logarithm, on the software
            // adapter) do not take a denormal number for what it is. The
            // arithmetic written out in the helpers does, and is compared.
            let denormal = [a, b, c, expected, got].into_iter().any(is_denormal);
            check == Check::Close && denormal
        },
    );
}

#[test]
#[ignore = "needs the shader compiler and a D3D12 device"]
fn on_a_device_loads_and_stores_of_every_width_reach_memory_at_any_address() {
    const GROUPS: u32 = 4;
    // Three pages in a row of which the last is not mapped, the pages on
    // both sides of 4 GiB, and a page of results.
    let near = 0x7_0000u64;
    let edge = 0xFFFF_0000u64;
    let mut m = Maker::new();
    let (lane, _) = lane_start(&mut m, 0);
    // Each lane reads at its own odd distance before the end of a page, so
    // that loads cross into the next page, mapped or not.
    let step = m.op(Op::Binary(BinaryOp::Mul, lane, Value::Word(3)));
    let mut results = Vec::new();
    for (base, pages) in [(near, 2u32), (edge, 1), (near + PAGE as u64, 1)] {
        let end = (base as u32)
            .wrapping_add(pages * PAGE as u32)
            .wrapping_sub(40);
        let carry_base = (base >> 32) as u32;
        let low = m.op(Op::Binary(BinaryOp::Add, step, Value::Word(end)));
        let wrapped = m.op(Op::Carry {
            a: step,
            b: Value::Word(end),
            carry_in: Value::Flag(false),
        });
        let carry = as_word(&mut m, wrapped);
        let high = m.op(Op::Binary(BinaryOp::Add, carry, Value::Word(carry_base)));
        let address = Address { low, high };
        for (width, words) in [(32u8, 4u8), (16, 3), (8, 5)] {
            results.extend(m.all(Op::LoadMemory {
                address,
                width,
                words,
            }));
        }
    }
    // Stores of each width at odd places of the lane's own 32 bytes, one
    // of them only in even lanes, and one to memory that is not mapped.
    let bytes = results.len() as u32 * 4 + 32;
    let out = results_at(&mut m, lane, bytes);
    let value = results[0];
    let even = m.op(Op::Binary(BinaryOp::And, lane, Value::Word(1)));
    let even = m.op(Op::Compare(CompareOp::Equal, even, Value::Word(0)));
    let at = |m: &mut Maker, offset: u32| {
        let low = m.op(Op::Binary(BinaryOp::Add, out.low, Value::Word(offset)));
        Address {
            low,
            high: Value::Word(0),
        }
    };
    let address = at(&mut m, 1);
    m.op(Op::StoreMemory {
        address,
        width: 8,
        values: vec![value, results[1]],
        guard: Value::Flag(true),
    });
    let address = at(&mut m, 5);
    m.op(Op::StoreMemory {
        address,
        width: 16,
        values: vec![results[2], results[3]],
        guard: Value::Flag(true),
    });
    let address = at(&mut m, 13);
    m.op(Op::StoreMemory {
        address,
        width: 32,
        values: vec![results[4], results[5]],
        guard: even,
    });
    m.op(Op::StoreMemory {
        address: Address {
            low: out.low,
            high: Value::Word(3),
        },
        width: 32,
        values: vec![value],
        guard: Value::Flag(true),
    });
    let address = at(&mut m, 32);
    m.op(Op::StoreMemory {
        address,
        width: 32,
        values: results,
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Compute, groups_of_64());

    let lanes = GROUPS * 64;
    let result_pages = (lanes * bytes).div_ceil(PAGE as u32) as u64;
    // The pages are given in an order that is not their order in memory,
    // so that a load across two of them has to look each up.
    let mut pages = vec![(near >> 16) + 1, edge >> 16, near >> 16, (edge >> 16) + 1];
    pages.extend((0..result_pages).map(|page| (RESULTS >> 16) + page));
    let mut memory = Memory::new(&pages);
    for page in 0..4 {
        for index in 0..PAGE {
            memory.pages[page].1[index] = mix((page * PAGE + index) as u64) as u8;
        }
    }
    let gpu = Gpu::new(pages.len());
    check(
        gpu.as_ref(),
        &function,
        &memory,
        &inputs(),
        GROUPS,
        |_, expected, got| expected == got,
    );
}

#[test]
#[ignore = "needs the shader compiler and a D3D12 device"]
fn on_a_device_buffers_are_read_and_written_through_their_descriptors() {
    const GROUPS: u32 = 4;
    let data = 0x3_0000_0000u64;
    let mut m = Maker::new();
    let (lane, operands) = lane_start(&mut m, 16);
    // The lane's own descriptor of the data page: its stride, record count
    // and bound check are its operands.
    let loaded = m.all(Op::LoadMemory {
        address: operands,
        width: 32,
        words: 4,
    });
    let descriptor = [
        Value::Word(data as u32 + 0x100),
        loaded[0],
        loaded[1],
        loaded[2],
    ];
    let position = BufferPosition {
        index: lane,
        offset: loaded[3],
    };
    let mut results = Vec::new();
    for (width, words) in [(32u8, 3u8), (16, 2), (8, 3)] {
        results.extend(m.all(Op::LoadBuffer {
            descriptor,
            position,
            width,
            words,
        }));
    }
    // Words in a row at a constant offset, and one at the lane's own: the
    // emitter reads these as whole words of memory, each once.
    for (offset, words) in [(Value::Word(0), 4u8), (Value::Word(5), 3), (loaded[3], 1)] {
        results.extend(m.all(Op::LoadBuffer {
            descriptor,
            position: BufferPosition {
                index: lane,
                offset,
            },
            width: 32,
            words,
        }));
    }
    // Every format, by a number in the code; and one by a number that
    // differs from lane to lane.
    let formats: Vec<u32> = (0..80)
        .filter(|&code| BufferFormat::from_code(code).is_some())
        .collect();
    let plain = [
        Value::Word(data as u32 + 0x100),
        Value::Word(3 | 1 << 16),
        Value::Word(u32::MAX),
        Value::Word(1 << 28),
    ];
    let offset = m.op(Op::Binary(BinaryOp::Mul, lane, Value::Word(5)));
    for &format in &formats {
        results.extend(m.all(Op::LoadFormat {
            descriptor: plain,
            position: BufferPosition {
                index: Value::Word(0),
                offset,
            },
            format: Value::Word(format),
        }));
    }
    let varying = m.op(Op::Binary(BinaryOp::And, loaded[3], Value::Word(63)));
    let varying = m.op(Op::Binary(BinaryOp::Add, varying, Value::Word(14)));
    results.extend(m.all(Op::LoadFormat {
        descriptor: plain,
        position: BufferPosition {
            index: Value::Word(0),
            offset,
        },
        format: varying,
    }));
    // Stores through a descriptor of the lane's own part of the results.
    let writable: Vec<u32> = formats
        .iter()
        .copied()
        .filter(|&code| {
            BufferFormat::from_code(code)
                .and_then(|format| format.encode([0; 4]))
                .is_some()
        })
        .collect();
    let store_bytes = (writable.len() as u32 + 2) * 16;
    let bytes = results.len() as u32 * 4 + store_bytes;
    let out = results_at(&mut m, lane, bytes);
    let own = [
        out.low,
        Value::Word(16 << 16),
        Value::Word(writable.len() as u32 + 1),
        Value::Word(0),
    ];
    for (slot, &format) in writable.iter().enumerate() {
        m.op(Op::StoreFormat {
            descriptor: own,
            position: BufferPosition {
                index: Value::Word(slot as u32),
                offset: Value::Word(0),
            },
            format: Value::Word(format),
            values: [results[0], results[1], results[2], loaded[3]],
            guard: Value::Flag(true),
        });
    }
    // The last record is written to by plain stores; one past it is
    // outside the buffer and has to be dropped.
    for (index, width) in [
        (writable.len() as u32, 16u8),
        (writable.len() as u32 + 1, 32),
    ] {
        m.op(Op::StoreBuffer {
            descriptor: own,
            position: BufferPosition {
                index: Value::Word(index),
                offset: Value::Word(3),
            },
            width,
            values: vec![results[3], results[4]],
            guard: Value::Flag(true),
        });
    }
    let low = m.op(Op::Binary(BinaryOp::Add, out.low, Value::Word(store_bytes)));
    m.op(Op::StoreMemory {
        address: Address {
            low,
            high: Value::Word(0),
        },
        width: 32,
        values: results,
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Compute, groups_of_64());

    let lanes = GROUPS * 64;
    let result_pages = (lanes * bytes).div_ceil(PAGE as u32) as u64;
    let mut pages = vec![OPERANDS >> 16, data >> 16];
    pages.extend((0..result_pages).map(|page| (RESULTS >> 16) + page));
    let mut memory = Memory::new(&pages);
    for index in 0..PAGE {
        memory.pages[1].1[index] = mix(index as u64 ^ 0x5555) as u8;
    }
    for lane in 0..lanes {
        let hash = mix(u64::from(lane));
        let at = OPERANDS + u64::from(lane * 16);
        // The high bits of the address (page 3 of 4 GiB), a stride of 0 to
        // 15 bytes; a record count around the lane's number; one of the
        // four bound checks; and an offset around the stride.
        let stride = (hash % 16) as u32;
        memory.set_word(at, 3 | stride << 16);
        memory.set_word(at + 4, (lane + (hash >> 8) as u32 % 3).wrapping_sub(1));
        memory.set_word(at + 8, ((hash >> 16) as u32 % 4) << 28 | 0x00F0_0FAC);
        memory.set_word(at + 12, (hash >> 24) as u32 % 20);
    }
    let gpu = Gpu::new(pages.len());
    check(
        gpu.as_ref(),
        &function,
        &memory,
        &inputs(),
        GROUPS,
        |_, expected, got| Check::Float.same(expected, got),
    );
}

/// A function with a random graph of `count` blocks. Every block mixes its
/// number into a sum, counts itself, and branches on a bit of the sum; once
/// forty blocks have run it only goes further on, so that every lane ends.
/// The end stores the sum and the count.
fn random_graph(seed: u64, count: usize) -> Option<Function> {
    let mut m = Maker::new();
    let (lane, _) = lane_start(&mut m, 0);
    let start = m.op(Op::Binary(BinaryOp::Mul, lane, Value::Word(0x9E37_79B9)));
    m.op(Op::Set(Var::Vector(0), start));
    m.op(Op::Set(Var::Vector(2), lane));
    m.end(Terminator::Jump(1));
    let end = count + 1;
    for block in 1..=count {
        let hash = mix(seed << 16 | block as u64);
        let sum = m.op(Op::Get(Var::Vector(0)));
        let sum = m.op(Op::Binary(BinaryOp::Mul, sum, Value::Word(33)));
        let sum = m.op(Op::Binary(BinaryOp::Add, sum, Value::Word(block as u32)));
        m.op(Op::Set(Var::Vector(0), sum));
        let ran = m.op(Op::Get(Var::Vector(1)));
        let ran = m.op(Op::Binary(BinaryOp::Add, ran, Value::Word(1)));
        m.op(Op::Set(Var::Vector(1), ran));
        let spent = m.op(Op::Compare(
            CompareOp::UnsignedGreater,
            ran,
            Value::Word(40),
        ));
        let bit = m.op(Op::Binary(
            BinaryOp::ShiftRight,
            sum,
            Value::Word((hash >> 40) as u32 % 16 + 3),
        ));
        let bit = m.op(Op::Binary(BinaryOp::And, bit, Value::Word(1)));
        let taken = m.op(Op::Compare(CompareOp::NotEqual, bit, Value::Word(0)));
        // Anywhere, the end included; or somewhere further on, which is
        // where a lane whose trips are spent always goes, so that it ends.
        let anywhere = |salt: u64| 1 + (mix(hash ^ salt) % (count as u64 + 1)) as usize;
        let onward = |salt: u64| block + 1 + (mix(hash ^ salt) % (end - block) as u64) as usize;
        let terminator = match hash % 3 {
            0 => Terminator::Branch {
                condition: spent,
                if_true: end,
                if_false: anywhere(1),
            },
            1 => {
                let either = m.op(Op::Flag(FlagOp::Or, taken, spent));
                Terminator::Branch {
                    condition: either,
                    if_true: onward(2),
                    if_false: anywhere(3),
                }
            }
            _ => {
                let going = m.op(Op::FlagNot(spent));
                let back = m.op(Op::Flag(FlagOp::And, taken, going));
                Terminator::Branch {
                    condition: back,
                    if_true: anywhere(4),
                    if_false: onward(5),
                }
            }
        };
        m.end(terminator);
    }
    let sum = m.op(Op::Get(Var::Vector(0)));
    let ran = m.op(Op::Get(Var::Vector(1)));
    let lane = m.op(Op::Get(Var::Vector(2)));
    let address = results_at(&mut m, lane, 8);
    m.op(Op::StoreMemory {
        address,
        width: 32,
        values: vec![sum, ran],
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let mut function = m.function(HardwareStage::Compute, groups_of_64());
    make_reducible(&mut function)?;
    function.verify().ok()?;
    Some(function)
}

#[test]
#[ignore = "needs the shader compiler and a D3D12 device"]
fn on_a_device_structured_control_flow_takes_the_paths_of_the_graph() {
    let pages = [RESULTS >> 16];
    let memory = Memory::new(&pages);
    let gpu = Gpu::new(pages.len());
    let mut checked = 0;
    let mut with_flag = 0;
    for seed in 0..60u64 {
        let Some(function) = random_graph(seed, 3 + (seed % 9) as usize) else {
            continue;
        };
        let emitted = emit(&function, Signatures::default(), &Options::default()).unwrap();
        assert!(!emitted.dispatched, "graph {seed} was not structured");
        // A lane that never ends would run out of fuel on the device and
        // of steps here; the graphs are made so that none does.
        with_flag += usize::from(emitted.source.contains("go = "));
        if !check(
            gpu.as_ref(),
            &function,
            &memory,
            &inputs(),
            2,
            |_, expected, got| expected == got,
        ) {
            return;
        }
        checked += 1;
    }
    assert!(
        checked >= 40,
        "only {checked} graphs could be made reducible"
    );
    assert!(
        with_flag >= 5,
        "only {with_flag} graphs needed a jump out of several scopes"
    );
}

#[test]
#[ignore = "needs the shader compiler and a D3D12 device"]
fn on_a_device_atomic_operations_change_memory_as_the_interpreter_does() {
    const GROUPS: u32 = 4;
    let targets = 0x2_0000_0000u64;
    let mut m = Maker::new();
    let (lane, operands) = lane_start(&mut m, 8);
    let loaded = m.all(Op::LoadMemory {
        address: operands,
        width: 32,
        words: 2,
    });
    let (a, b) = (loaded[0], loaded[1]);
    // Eight words that the lanes of every group work on together, for each
    // operation whose result does not depend on the order: the lanes pick
    // their word by an operand.
    let word = m.op(Op::Binary(BinaryOp::And, a, Value::Word(7)));
    let shared = m.op(Op::Binary(BinaryOp::ShiftLeft, word, Value::Word(2)));
    let even = m.op(Op::Binary(BinaryOp::And, lane, Value::Word(1)));
    let even = m.op(Op::Compare(CompareOp::Equal, even, Value::Word(0)));
    let together = [
        AtomicOp::Add,
        AtomicOp::Sub,
        AtomicOp::Increment,
        AtomicOp::Decrement,
        AtomicOp::MinUnsigned,
        AtomicOp::MaxUnsigned,
        AtomicOp::And,
        AtomicOp::Or,
        AtomicOp::Xor,
    ];
    // A buffer over the page of those words: 4 bytes to a record, 512
    // records, bound by index and offset.
    let view = [
        Value::Word(targets as u32),
        Value::Word((targets >> 32) as u32 | 4 << 16),
        Value::Word(512),
        Value::Word(0),
    ];
    for (index, &op) in together.iter().enumerate() {
        // By address, with a low bit set that has to be ignored; and
        // through the buffer, by every second lane.
        let low = m.op(Op::Binary(
            BinaryOp::Add,
            shared,
            Value::Word(targets as u32 + 0x40 * index as u32 + 1),
        ));
        m.op(Op::Atomic {
            op,
            place: AtomicPlace::Memory(Address {
                low,
                high: Value::Word((targets >> 32) as u32),
            }),
            value: b,
            compare: Value::Word(0),
            guard: Value::Flag(true),
        });
        let record = m.op(Op::Binary(
            BinaryOp::Add,
            word,
            Value::Word(0x100 + 0x10 * index as u32),
        ));
        m.op(Op::Atomic {
            op,
            place: AtomicPlace::Buffer {
                descriptor: view,
                position: BufferPosition {
                    index: record,
                    offset: Value::Word(0),
                },
            },
            value: b,
            compare: Value::Word(0),
            guard: even,
        });
    }
    // The operations whose result does depend on the order, and the words
    // the operations give back, on words of the lane's own: its results.
    let bytes = 4 * 16;
    let out = results_at(&mut m, lane, bytes);
    let own = |m: &mut Maker, word: u32| Address {
        low: m.op(Op::Binary(BinaryOp::Add, out.low, Value::Word(4 * word))),
        high: Value::Word(0),
    };
    let mut results = Vec::new();
    for (word, (op, compare)) in [
        (AtomicOp::Swap, Value::Word(0)),
        // The words start as the lane's number: one comparison holds, the
        // other does not.
        (AtomicOp::CompareSwap, lane),
        (AtomicOp::CompareSwap, a),
        (AtomicOp::Add, Value::Word(0)),
        (AtomicOp::MaxUnsigned, Value::Word(0)),
    ]
    .into_iter()
    .enumerate()
    {
        let place = AtomicPlace::Memory(own(&mut m, word as u32));
        results.push(m.op(Op::Atomic {
            op,
            place,
            value: b,
            compare,
            guard: Value::Flag(true),
        }));
    }
    // Nothing happens, and zero comes back: with the guard clear, outside
    // the buffer, and where no memory is mapped.
    let place = AtomicPlace::Memory(own(&mut m, 5));
    results.push(m.op(Op::Atomic {
        op: AtomicOp::Add,
        place,
        value: b,
        compare: Value::Word(0),
        guard: Value::Flag(false),
    }));
    results.push(m.op(Op::Atomic {
        op: AtomicOp::Add,
        place: AtomicPlace::Buffer {
            descriptor: view,
            position: BufferPosition {
                index: Value::Word(512),
                offset: Value::Word(0),
            },
        },
        value: b,
        compare: Value::Word(0),
        guard: Value::Flag(true),
    }));
    results.push(m.op(Op::Atomic {
        op: AtomicOp::Or,
        place: AtomicPlace::Memory(Address {
            low: a,
            high: Value::Word(9),
        }),
        value: b,
        compare: Value::Word(0),
        guard: Value::Flag(true),
    }));
    let address = own(&mut m, 8);
    m.op(Op::StoreMemory {
        address,
        width: 32,
        values: results,
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(HardwareStage::Compute, groups_of_64());

    let lanes = GROUPS * 64;
    let result_pages = (lanes * bytes).div_ceil(PAGE as u32) as u64;
    let mut pages = vec![OPERANDS >> 16, targets >> 16];
    pages.extend((0..result_pages).map(|page| (RESULTS >> 16) + page));
    let mut memory = Memory::new(&pages);
    for index in 0..PAGE {
        memory.pages[1].1[index] = mix(index as u64 ^ 0x1234) as u8;
    }
    for lane in 0..lanes {
        let at = OPERANDS + u64::from(lane * 8);
        memory.set_word(at, operand(lane, 0));
        memory.set_word(at + 4, operand(lane, 1));
        for word in 0..8 {
            memory.set_word(RESULTS + u64::from(lane * bytes + 4 * word), lane);
        }
    }
    let gpu = Gpu::new(pages.len());
    check(
        gpu.as_ref(),
        &function,
        &memory,
        &inputs(),
        GROUPS,
        |_, expected, got| expected == got,
    );
}

#[test]
#[ignore = "needs the shader compiler and a D3D12 device"]
fn on_a_device_threads_see_the_shared_memory_as_the_lanes_of_a_wave_do() {
    const GROUPS: u32 = 3;
    let mut m = Maker::new();
    let (lane, operands) = lane_start(&mut m, 8);
    let thread = m.op(Op::Input(Input::ThreadId(0)));
    let loaded = m.all(Op::LoadMemory {
        address: operands,
        width: 32,
        words: 2,
    });
    let (a, b) = (loaded[0], loaded[1]);
    let first = m.op(Op::Compare(CompareOp::Equal, thread, Value::Word(0)));
    // Every thread writes two words at its own place, and bytes and halves
    // at odd places of its own further on. The first thread also clears
    // the words the atomic operations will work on.
    let place = m.op(Op::Binary(BinaryOp::ShiftLeft, thread, Value::Word(3)));
    m.op(Op::StoreShared {
        offset: place,
        width: 32,
        values: vec![a, b],
        guard: Value::Flag(true),
    });
    let odd = m.op(Op::Binary(BinaryOp::Add, place, Value::Word(0x401)));
    m.op(Op::StoreShared {
        offset: odd,
        width: 8,
        values: vec![a],
        guard: Value::Flag(true),
    });
    let odd_half = m.op(Op::Binary(BinaryOp::Add, place, Value::Word(0x403)));
    m.op(Op::StoreShared {
        offset: odd_half,
        width: 16,
        values: vec![b, a],
        guard: Value::Flag(true),
    });
    let cleared = [0x0u32, 0xFFFF_FFFF, 0, 0, 0xFFFF_FFFF, 0, 0x1234_5678, 0];
    m.op(Op::StoreShared {
        offset: Value::Word(0x800),
        width: 32,
        values: cleared.iter().map(|&word| Value::Word(word)).collect(),
        guard: first,
    });
    // Past the end, and across it: dropped.
    m.op(Op::StoreShared {
        offset: Value::Word(0x1000),
        width: 32,
        values: vec![a],
        guard: Value::Flag(true),
    });
    m.op(Op::StoreShared {
        offset: Value::Word(0xFFE),
        width: 32,
        values: vec![a],
        guard: Value::Flag(true),
    });
    // Each thread reads what the thread opposite wrote: on the console a
    // later instruction of the same wave, here after a barrier the emitter
    // adds.
    let opposite = m.op(Op::Binary(BinaryOp::Sub, Value::Word(63), thread));
    let there = m.op(Op::Binary(BinaryOp::ShiftLeft, opposite, Value::Word(3)));
    let mut results = m.all(Op::LoadShared {
        offset: there,
        width: 32,
        words: 2,
    });
    let odd = m.op(Op::Binary(BinaryOp::Add, there, Value::Word(0x401)));
    results.extend(m.all(Op::LoadShared {
        offset: odd,
        width: 8,
        words: 1,
    }));
    let odd_half = m.op(Op::Binary(BinaryOp::Add, there, Value::Word(0x403)));
    results.extend(m.all(Op::LoadShared {
        offset: odd_half,
        width: 16,
        words: 2,
    }));
    // A word across two words of the memory, and reads past the end.
    let across = m.op(Op::Binary(BinaryOp::Add, there, Value::Word(3)));
    results.extend(m.all(Op::LoadShared {
        offset: across,
        width: 32,
        words: 1,
    }));
    for offset in [0xFFE, 0x1000, 0xFFFF_FFFF] {
        results.extend(m.all(Op::LoadShared {
            offset: Value::Word(offset),
            width: 32,
            words: 1,
        }));
    }
    // The atomic operations of all threads on the words the first cleared,
    // one word each; then every thread reads what they came to.
    for (index, op) in [
        AtomicOp::Add,
        AtomicOp::MinUnsigned,
        AtomicOp::MaxUnsigned,
        AtomicOp::Or,
        AtomicOp::And,
        AtomicOp::Xor,
        AtomicOp::Sub,
        AtomicOp::Increment,
    ]
    .into_iter()
    .enumerate()
    {
        m.op(Op::Atomic {
            op,
            place: AtomicPlace::Shared(Value::Word(0x800 + 4 * index as u32)),
            value: a,
            compare: Value::Word(0),
            guard: Value::Flag(true),
        });
    }
    // Past the end: nothing, and zero back.
    results.push(m.op(Op::Atomic {
        op: AtomicOp::Add,
        place: AtomicPlace::Shared(Value::Word(0x1000)),
        value: a,
        compare: Value::Word(0),
        guard: Value::Flag(true),
    }));
    results.extend(m.all(Op::LoadShared {
        offset: Value::Word(0x800),
        width: 32,
        words: 8,
    }));
    // An exchange and a compare-and-swap on the thread's own word give
    // back what was there.
    m.op(Op::Barrier);
    for (op, compare) in [(AtomicOp::Swap, Value::Word(0)), (AtomicOp::CompareSwap, b)] {
        results.push(m.op(Op::Atomic {
            op,
            place: AtomicPlace::Shared(place),
            value: lane,
            compare,
            guard: Value::Flag(true),
        }));
    }
    results.extend(m.all(Op::LoadShared {
        offset: place,
        width: 32,
        words: 1,
    }));
    let bytes = results.len() as u32 * 4;
    let address = results_at(&mut m, lane, bytes);
    m.op(Op::StoreMemory {
        address,
        width: 32,
        values: results,
        guard: Value::Flag(true),
    });
    m.end(Terminator::Return);
    let function = m.function(
        HardwareStage::Compute,
        Interface {
            shared_bytes: 0x1000,
            ..groups_of_64()
        },
    );
    // The program has one barrier of its own; the emitter adds one at every
    // other change between writing, reading and atomic operations.
    let source = emit(&function, Signatures::default(), &Options::default())
        .unwrap()
        .source;
    assert_eq!(
        source.matches("GroupMemoryBarrierWithGroupSync();").count(),
        5,
        "{source}"
    );

    let lanes = GROUPS * 64;
    let result_pages = (lanes * bytes).div_ceil(PAGE as u32) as u64;
    let mut pages = vec![OPERANDS >> 16];
    pages.extend((0..result_pages).map(|page| (RESULTS >> 16) + page));
    let mut memory = Memory::new(&pages);
    for lane in 0..lanes {
        let at = OPERANDS + u64::from(lane * 8);
        memory.set_word(at, operand(lane, 0));
        memory.set_word(at + 4, operand(lane, 1));
    }
    let gpu = Gpu::new(pages.len());
    check(
        gpu.as_ref(),
        &function,
        &memory,
        &inputs(),
        GROUPS,
        |_, expected, got| expected == got,
    );
}
