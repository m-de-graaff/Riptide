//! Scattered shader reads from in-place title memory against video memory, and a shader
//! write into in-place memory (open questions Q2 and Q3).

use crate::buffers::{MIB, committed, pattern, read_back};
use crate::device::{Gpu, submit};
use crate::inplace::InPlace;
use crate::report::{Facts, Place, RandomRead};
use crate::timing::{RUNS, best};
use std::mem::ManuallyDrop;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{PCSTR, s};

const HLSL: &str = r#"
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
cbuffer Args : register(b0) { uint words; uint reads; uint dst_base; };

[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x * 2654435761u + 12345u;
    uint sum = 0;
    for (uint i = 0; i < reads; i++) {
        x = x * 1664525u + 1013904223u;
        sum += src.Load((x % words) * 4);
    }
    dst.Store(dst_base + id.x * 4, sum);
}
"#;

const SRC_BYTES: u64 = 256 * MIB;
const THREADS: u32 = 65_536;
const READS: u32 = 256;
const OUT_BYTES: u64 = THREADS as u64 * 4;
/// Where the in-place run stores its sums: just past the source data.
const OUT_IN_PLACE: u64 = SRC_BYTES;

fn blob_bytes(blob: &ID3DBlob) -> Vec<u8> {
    // SAFETY: a blob's pointer is valid for its reported size while the blob lives.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }.to_vec()
}

pub fn compile() -> Result<Vec<u8>, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source pointer and length describe `HLSL`; the out-pointers are valid.
    let result = unsafe {
        D3DCompile(
            HLSL.as_ptr().cast(),
            HLSL.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            s!("main"),
            s!("cs_5_0"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    match (result, code) {
        (Ok(()), Some(code)) => Ok(blob_bytes(&code)),
        (r, _) => Err(format!(
            "{r:?}: {}",
            errors.map(|e| String::from_utf8_lossy(&blob_bytes(&e)).into_owned()).unwrap_or_default()
        )),
    }
}

struct Pass {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
}

fn build(gpu: &Gpu) -> windows::core::Result<Pass> {
    let code = compile().map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e))?;
    let descriptor = |kind| D3D12_ROOT_PARAMETER {
        ParameterType: kind,
        Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let params = [
        descriptor(D3D12_ROOT_PARAMETER_TYPE_SRV),
        descriptor(D3D12_ROOT_PARAMETER_TYPE_UAV),
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 3 },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: params.len() as u32,
        pParameters: params.as_ptr(),
        NumStaticSamplers: 0,
        pStaticSamplers: std::ptr::null(),
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    // SAFETY: `desc` points at `params`, which outlives the call; out-pointers are valid; the
    // pipeline description borrows `code` and a root signature reference released below.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.expect("serialised root signature");
        let root: ID3D12RootSignature = gpu.device.CreateRootSignature(0, &blob_bytes(&blob))?;
        let pso = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
            NodeMask: 0,
            CachedPSO: Default::default(),
            Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
        };
        let pipeline = gpu.device.CreateComputePipelineState(&pso);
        drop(ManuallyDrop::into_inner(pso.pRootSignature));
        Ok(Pass { root, pipeline: pipeline? })
    }
}

/// Runs the shader once. `dst_base` is the byte offset in `dst` where sums are stored.
fn dispatch(gpu: &Gpu, pass: &Pass, src: &ID3D12Resource, dst: &ID3D12Resource, dst_base: u32) -> windows::core::Result<()> {
    let args = [(SRC_BYTES / 4) as u32, READS, dst_base];
    submit(gpu, |list| {
        // SAFETY: the root signature has parameters 0 (SRV), 1 (UAV) and 2 (three constants);
        // `args` outlives the call; both buffers outlive the submission, which is waited for.
        unsafe {
            list.SetComputeRootSignature(&pass.root);
            list.SetPipelineState(&pass.pipeline);
            list.SetComputeRootShaderResourceView(0, src.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(1, dst.GetGPUVirtualAddress());
            list.SetComputeRoot32BitConstants(2, 3, args.as_ptr().cast(), 0);
            list.Dispatch(THREADS / 64, 1, 1);
        }
    })
}

fn time_reads(gpu: &Gpu, pass: &Pass, src: &ID3D12Resource, dst: &ID3D12Resource) -> windows::core::Result<f64> {
    let mut samples = Vec::with_capacity(RUNS);
    for run in 0..=RUNS {
        let t = Instant::now();
        dispatch(gpu, pass, src, dst, 0)?;
        if run > 0 {
            samples.push(t.elapsed());
        }
    }
    Ok(best(&samples).as_nanos() as f64 / (THREADS as f64 * READS as f64))
}

pub fn check_gpu_read(gpu: &Gpu, mem: &InPlace, facts: &mut Facts) {
    assert!(mem.bytes >= SRC_BYTES + OUT_BYTES);
    if let Err(e) = run(gpu, mem, facts) {
        facts.notes.push(format!("shader read check: {e}"));
    }
}

fn run(gpu: &Gpu, mem: &InPlace, facts: &mut Facts) -> windows::core::Result<()> {
    {
        let pass = build(gpu)?;
        mem.slice(0, SRC_BYTES).copy_from_slice(&pattern(0x44, SRC_BYTES as usize));

        let video_src = committed(gpu, D3D12_HEAP_TYPE_DEFAULT, SRC_BYTES, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON)?;
        submit(gpu, |list| {
            // SAFETY: both buffers are at least SRC_BYTES long.
            unsafe { list.CopyBufferRegion(&video_src, 0, &mem.buffer, 0, SRC_BYTES) }
        })?;
        let out = |gpu: &Gpu| {
            committed(gpu, D3D12_HEAP_TYPE_DEFAULT, OUT_BYTES, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON)
        };
        let (out_in_place, out_video) = (out(gpu)?, out(gpu)?);

        let ns = time_reads(gpu, &pass, &mem.buffer, &out_in_place)?;
        facts.gpu_random_read.push(RandomRead { source: Place::InPlaceRam, ns_per_read: ns });
        let ns = time_reads(gpu, &pass, &video_src, &out_video)?;
        facts.gpu_random_read.push(RandomRead { source: Place::Video, ns_per_read: ns });

        let expected = read_back(gpu, &out_video, 0, OUT_BYTES)?;
        facts.in_place_reads_match = Some(read_back(gpu, &out_in_place, 0, OUT_BYTES)? == expected);

        // Q3: the same sums, stored by the shader into in-place memory, read by the CPU.
        mem.slice(OUT_IN_PLACE, OUT_BYTES).fill(0);
        facts.in_place_uav = Some(match dispatch(gpu, &pass, &video_src, &mem.buffer, OUT_IN_PLACE as u32) {
            Ok(()) => mem.slice(OUT_IN_PLACE, OUT_BYTES) == expected.as_slice(),
            Err(e) => {
                facts.notes.push(format!("shader write to in-place memory: {e}"));
                false
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_compiles() {
        let code = compile().unwrap();
        assert_eq!(&code[..4], b"DXBC");
    }

    #[test]
    #[ignore = "needs the target machine"]
    fn shader_reads_in_place_memory_correctly() {
        let gpu = crate::device::open(false).unwrap();
        let mem = crate::inplace::open(&gpu, crate::buffers::GIB, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS).unwrap();
        let mut facts = Facts::default();
        check_gpu_read(&gpu, &mem, &mut facts);
        assert_eq!(facts.in_place_reads_match, Some(true), "notes: {:?}", facts.notes);
        assert_eq!(facts.gpu_random_read.len(), 2);
        eprintln!("{:#?} uav: {:?}", facts.gpu_random_read, facts.in_place_uav);
    }
}
