//! How fast the GPU moves bulk data between in-place title memory and video memory
//! (open question Q2).

use crate::buffers::{GIB, MIB, committed, pattern};
use crate::device::{Gpu, submit};
use crate::inplace::InPlace;
use crate::report::{Facts, GpuCopy, Place};
use crate::timing::{RUNS, best, gib_per_s};
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::*;

const BYTES: u64 = 256 * MIB;

fn time_copy(gpu: &Gpu, dst: &ID3D12Resource, src: &ID3D12Resource) -> windows::core::Result<f64> {
    let mut samples = Vec::with_capacity(RUNS);
    for run in 0..=RUNS {
        let t = Instant::now();
        submit(gpu, |list| {
            // SAFETY: both buffers are at least BYTES long.
            unsafe { list.CopyBufferRegion(dst, 0, src, 0, BYTES) }
        })?;
        if run > 0 {
            samples.push(t.elapsed());
        }
    }
    Ok(gib_per_s(BYTES, best(&samples)))
}

pub fn check_gpu_copy(gpu: &Gpu, mem: &InPlace, facts: &mut Facts) {
    assert!(mem.bytes >= BYTES && GIB >= BYTES);
    // Touch every page first, so the copies time the bus and not first-use page faults.
    mem.slice(0, BYTES).copy_from_slice(&pattern(0x33, BYTES as usize));
    let video = |gpu: &Gpu| committed(gpu, D3D12_HEAP_TYPE_DEFAULT, BYTES, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COMMON);
    let (a, b) = match (video(gpu), video(gpu)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            facts.notes.push(format!("video buffers for copy timing: {e}"));
            return;
        }
    };
    let cases = [
        (Place::InPlaceRam, Place::Video, &a, &mem.buffer),
        (Place::Video, Place::Video, &b, &a),
        (Place::Video, Place::InPlaceRam, &mem.buffer, &a),
    ];
    for (from, to, dst, src) in cases {
        match time_copy(gpu, dst, src) {
            Ok(gib_s) => facts.gpu_copy.push(GpuCopy { from, to, gib_s }),
            Err(e) => facts.notes.push(format!("copy {from:?} to {to:?}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs the target machine"]
    fn times_three_copy_directions() {
        let gpu = crate::device::open(false).unwrap();
        let mem = crate::inplace::open(&gpu, GIB, D3D12_RESOURCE_FLAG_NONE).unwrap();
        let mut facts = Facts::default();
        check_gpu_copy(&gpu, &mem, &mut facts);
        assert_eq!(facts.gpu_copy.len(), 3, "notes: {:?}", facts.notes);
        assert!(facts.gpu_copy.iter().all(|c| c.gib_s > 0.0));
        eprintln!("{:#?}", facts.gpu_copy);
    }
}
