//! Measures whether the target machine supports Riptide's in-place memory design, before
//! anything depends on it. See README.md in this folder.

mod bandwidth;
mod buffers;
mod device;
mod gpucopy;
mod gpuread;
mod guard;
mod inplace;
mod report;
mod timing;

use std::process::ExitCode;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_FLAG_NONE,
};

/// A 1 GiB in-place block for the timed checks, with shader writes allowed when the driver
/// takes that flag.
fn work_block(gpu: &device::Gpu, facts: &mut report::Facts) -> Option<inplace::InPlace> {
    match inplace::open(
        gpu,
        buffers::GIB,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    ) {
        Ok(mem) => Some(mem),
        Err(e) => {
            facts.notes.push(format!(
                "in-place buffer with unordered access refused: {e}"
            ));
            facts.in_place_uav = Some(false);
            inplace::open(gpu, buffers::GIB, D3D12_RESOURCE_FLAG_NONE)
                .map_err(|e| facts.notes.push(format!("in-place work block: {e}")))
                .ok()
        }
    }
}

fn main() -> ExitCode {
    let debug = std::env::args().skip(1).any(|a| a == "--debug");
    let mut gpu = match device::open(debug) {
        Ok(gpu) => gpu,
        Err(e) => {
            eprintln!("riptide-probe: could not open a D3D12 device: {e}");
            return ExitCode::from(2);
        }
    };
    let mut facts = report::Facts {
        adapter: gpu.info.name.clone(),
        driver_version: gpu.driver_version.clone(),
        dedicated_video_memory_mib: gpu.info.dedicated_video_memory >> 20,
        ..Default::default()
    };
    device::query_features(&gpu, &mut facts);
    inplace::check_in_place(&gpu, &mut facts);

    if facts.in_place_1gib == Some(true) {
        if let Some(mem) = work_block(&gpu, &mut facts) {
            gpucopy::check_gpu_copy(&gpu, &mem, &mut facts);
            guard::check_guard(&gpu, &mem, &mut facts);
            let uav_already_refused = facts.in_place_uav == Some(false);
            gpuread::check_gpu_read(&gpu, &mem, &mut facts);
            if uav_already_refused {
                facts.in_place_uav = Some(false);
            }
        }
        // A refused shader write can remove the device; the last check gets a fresh one.
        // SAFETY: plain COM query.
        if unsafe { gpu.device.GetDeviceRemovedReason() }.is_err() {
            match device::open(debug) {
                Ok(fresh) => gpu = fresh,
                Err(e) => facts
                    .notes
                    .push(format!("device lost and not reopened: {e}")),
            }
        }
    }
    bandwidth::check_bandwidth(&gpu, &mut facts);

    let report = report::build(facts);
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("report serialises")
    );
    if report.pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
