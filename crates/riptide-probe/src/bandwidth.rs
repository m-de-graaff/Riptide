//! CPU write and read bandwidth to cached system memory and to video memory mapped through
//! Resizable BAR (open question Q5).

use crate::buffers::{MIB, committed, map};
use crate::device::Gpu;
use crate::report::{Bandwidth, Facts, MemoryKind};
use crate::timing::{RUNS, best, gib_per_s};
use std::hint::black_box;
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_HEAP_TYPE_GPU_UPLOAD, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON,
};

/// Small, because reads of video memory are expected to run at tens of MiB per second.
const UPLOAD_BYTES: usize = (32 * MIB) as usize;
const CACHED_BYTES: usize = (256 * MIB) as usize;

/// Times writing then reading `len` bytes at `dst`: one warm-up, then the best of RUNS.
pub fn measure(dst: *mut u8, len: usize) -> (f64, f64) {
    let src = vec![0x5Au8; len];
    let mut writes = Vec::with_capacity(RUNS);
    let mut reads = Vec::with_capacity(RUNS);
    for run in 0..=RUNS {
        let t = Instant::now();
        // SAFETY: the caller guarantees `dst` is valid for `len` writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, len) };
        black_box(dst);
        let w = t.elapsed();

        let t = Instant::now();
        // SAFETY: as above, and `dst` is 8-byte aligned (heap allocations and mapped buffers are).
        let words = unsafe { std::slice::from_raw_parts(dst.cast::<u64>(), len / 8) };
        black_box(words.iter().fold(0u64, |a, &x| a.wrapping_add(x)));
        let r = t.elapsed();

        if run > 0 {
            writes.push(w);
            reads.push(r);
        }
    }
    (gib_per_s(len as u64, best(&writes)), gib_per_s(len as u64, best(&reads)))
}

pub fn check_bandwidth(gpu: &Gpu, facts: &mut Facts) {
    let mut cached = vec![0u8; CACHED_BYTES];
    let (w, r) = measure(cached.as_mut_ptr(), CACHED_BYTES);
    facts.cpu_bandwidth.push(Bandwidth { memory: MemoryKind::CachedSystem, write_gib_s: w, read_gib_s: r });

    if !facts.gpu_upload_heap_supported {
        facts.notes.push("GPU upload heaps not reported as supported; Q5 not measured".into());
        return;
    }
    let buffer = committed(gpu, D3D12_HEAP_TYPE_GPU_UPLOAD, UPLOAD_BYTES as u64, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON);
    match buffer.and_then(|b| Ok((map(&b)?, b))) {
        Ok((p, _buffer)) => {
            let (w, r) = measure(p, UPLOAD_BYTES);
            facts.cpu_bandwidth.push(Bandwidth { memory: MemoryKind::GpuUpload, write_gib_s: w, read_gib_s: r });
        }
        Err(e) => facts.notes.push(format!("GPU upload buffer for bandwidth: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_cached_memory() {
        let mut v = vec![0u8; 16 << 20];
        let (w, r) = measure(v.as_mut_ptr(), v.len());
        assert!(w > 0.0 && r > 0.0);
    }

    #[test]
    #[ignore = "needs the target machine"]
    fn measures_gpu_upload_memory_when_supported() {
        let gpu = crate::device::open(false).unwrap();
        let mut facts = Facts::default();
        crate::device::query_features(&gpu, &mut facts);
        check_bandwidth(&gpu, &mut facts);
        assert_eq!(facts.cpu_bandwidth.len(), if facts.gpu_upload_heap_supported { 2 } else { 1 });
        eprintln!("{:#?} notes: {:?}", facts.cpu_bandwidth, facts.notes);
    }
}
