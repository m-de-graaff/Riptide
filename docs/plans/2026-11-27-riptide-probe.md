# riptide-probe Implementation Plan

> **To execute:** use the `executing-plans` skill. Steps use `- [ ]` for tracking.

**Goal:** A command-line tool that answers open questions Q1 to Q5 in `docs/design/2026-11-27-architecture.md` on the target machine, and prints the answers as JSON.

**Architecture:** One binary crate, `crates/riptide-probe`. Pure logic (judging the report, picking the adapter, timing arithmetic) is unit-tested on any Windows machine. Hardware code (the D3D12 device, the in-place heap, GPU copies, a compute shader, write-protect faults) only runs on the target machine. Each hardware check records its outcome in a `Facts` struct instead of aborting, so one failed check does not hide the others. `report::evaluate` turns `Facts` into pass/fail checks, and the exit code is 0 only when every required check passes.

**Tech stack:** Rust 1.97.1 (pinned in `rust-toolchain.toml`), edition 2024, `windows` 0.62.2, `serde` 1, `serde_json` 1. Shaders are compiled with `D3DCompile` from the `d3dcompiler_47.dll` that ships with Windows.

## Global Constraints

- Target machine: Radeon RX 7900 XTX (24 GB), Ryzen 7 7800X3D, 32 GB DDR5, Windows 11.
- Dependencies of `riptide-probe`: `windows = "=0.62.2"`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`. No others.
- `windows` features: `Win32_Foundation`, `Win32_Security`, `Win32_Graphics_Direct3D`, `Win32_Graphics_Direct3D_Fxc`, `Win32_Graphics_Direct3D12`, `Win32_Graphics_Dxgi`, `Win32_Graphics_Dxgi_Common`, `Win32_System_Memory`, `Win32_System_Kernel`, `Win32_System_Diagnostics_Debug`.
- Workspace lints apply: `unsafe_op_in_unsafe_fn = "deny"`, `clippy::undocumented_unsafe_blocks = "warn"`. Every `unsafe` block has a `// SAFETY:` comment on the line above it.
- Plain `cargo build` and `cargo test` are portable. `cargo build-7800x3d` makes the Zen 4 release build, which does not run on CPUs without Zen 4's instruction set. On the target machine either works; the measurements are taken with the release build.
- No game, console or SDK file is ever committed. Probe output goes to `local/probe/`, and `local/` is in `.gitignore`.
- Tests that need the target machine are marked `#[ignore = "needs the target machine"]` and run with `cargo test -p riptide-probe --release -- --ignored`. Plain `cargo test -p riptide-probe` must pass on any Windows machine.
- Sizes: `TILE = 65_536`, `MIB = 1 << 20`, `GIB = 1 << 30`. The title block is 14 GiB (the console gives a title 13.5 GB). `TITLE_SPACE_BITS = 34`.
- No tile mappings anywhere. Vice-Tay's notes record that tile mappings into a heap opened from a section remove the device.
- A buffer placed in a heap opened from a section must have `D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER`.
- The 14 GiB section is committed, so it is charged against the page file even though only the pages touched use RAM. If the section cannot be created, the report says so; that is a finding about the machine's page file, not a bug.
- The repository has no commits yet. Before Task 1, ask the user whether to commit the scaffold. Each task's commit step runs only once the user has agreed to commits.

## File map

| File | Responsibility |
| --- | --- |
| `crates/riptide-probe/Cargo.toml` | Dependencies (modified: the scaffold's `riptide-trace` dependency is removed; the probe does not need it yet) |
| `crates/riptide-probe/src/main.rs` | Argument parsing, running the checks in order, printing JSON, exit code |
| `crates/riptide-probe/src/report.rs` | `Facts`, `Check`, `Report`, `evaluate`, `all_required_pass`, `build`. Pure |
| `crates/riptide-probe/src/device.rs` | Adapter selection, device and queue creation, feature queries, `submit` |
| `crates/riptide-probe/src/buffers.rs` | Buffer descriptions, committed buffers, mapping, `upload`, `read_back`, test patterns |
| `crates/riptide-probe/src/inplace.rs` | A committed section opened as a heap with one buffer; the Q1 check |
| `crates/riptide-probe/src/timing.rs` | `gib_per_s`, `best`. Pure |
| `crates/riptide-probe/src/gpucopy.rs` | GPU copy throughput between in-place memory and video memory (Q2) |
| `crates/riptide-probe/src/gpuread.rs` | A compute shader doing scattered reads and an in-place write (Q2, Q3) |
| `crates/riptide-probe/src/guard.rs` | Write-protect fault cost, and whether GPU writes pass a guard (Q4) |
| `crates/riptide-probe/src/bandwidth.rs` | CPU bandwidth to cached memory and GPU upload heap memory (Q5) |
| `crates/riptide-probe/README.md` | How to run the probe (modified) |

---

### Task 1: Report model and requirement checks

**Files:**
- Modify: `crates/riptide-probe/Cargo.toml`
- Modify: `crates/riptide-probe/README.md`
- Create: `crates/riptide-probe/src/report.rs`
- Modify: `crates/riptide-probe/src/main.rs`
- Test: `crates/riptide-probe/src/report.rs` (unit tests module)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub const TITLE_SPACE_BITS: u32 = 34;`
  - `pub enum Place { InPlaceRam, Video }` and `pub enum MemoryKind { CachedSystem, GpuUpload }` (Serialize, Clone, Copy, Debug, PartialEq, Eq; serialised in snake_case)
  - `pub struct GpuCopy { pub from: Place, pub to: Place, pub gib_s: f64 }`
  - `pub struct RandomRead { pub source: Place, pub ns_per_read: f64 }`
  - `pub struct GuardCost { pub fault_us: f64, pub protect_us_per_gib: f64 }`
  - `pub struct Bandwidth { pub memory: MemoryKind, pub write_gib_s: f64, pub read_gib_s: f64 }`
  - `pub struct Facts { pub adapter: String, pub driver_version: String, pub dedicated_video_memory_mib: u64, pub va_bits_per_resource: u32, pub va_bits_per_process: u32, pub in_place_1gib: Option<bool>, pub in_place_14gib: Option<bool>, pub gpu_copy: Vec<GpuCopy>, pub gpu_random_read: Vec<RandomRead>, pub in_place_reads_match: Option<bool>, pub in_place_uav: Option<bool>, pub guard: Option<GuardCost>, pub gpu_writes_bypass_guard: Option<bool>, pub options16_known: bool, pub gpu_upload_heap_supported: bool, pub cpu_bandwidth: Vec<Bandwidth>, pub notes: Vec<String> }` (Serialize, Debug, Default, Clone)
  - `pub struct Check { pub id: &'static str, pub question: &'static str, pub required: bool, pub passed: bool, pub detail: String }`
  - `pub struct Report { pub facts: Facts, pub checks: Vec<Check>, pub pass: bool }`
  - `pub fn evaluate(facts: &Facts) -> Vec<Check>`, `pub fn all_required_pass(checks: &[Check]) -> bool`, `pub fn build(facts: Facts) -> Report`

- [ ] **Step 1: Set the dependencies**

Replace the `[dependencies]` section of `crates/riptide-probe/Cargo.toml` with:

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
windows = { version = "=0.62.2", features = [
    "Win32_Foundation",
    "Win32_Security",
    "Win32_Graphics_Direct3D",
    "Win32_Graphics_Direct3D_Fxc",
    "Win32_Graphics_Direct3D12",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_System_Memory",
    "Win32_System_Kernel",
    "Win32_System_Diagnostics_Debug",
] }
```

In `crates/riptide-probe/README.md`, replace the line `- \`riptide-trace\`` under "Depends on" with `Nothing in this workspace. (It will use \`riptide-trace\` once that exists.)`

- [ ] **Step 2: Write the failing tests**

Create `crates/riptide-probe/src/report.rs` with only the tests module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Facts {
        Facts {
            va_bits_per_resource: 40,
            va_bits_per_process: 47,
            in_place_1gib: Some(true),
            in_place_14gib: Some(true),
            in_place_reads_match: Some(true),
            in_place_uav: Some(true),
            guard: Some(GuardCost { fault_us: 2.0, protect_us_per_gib: 900.0 }),
            gpu_writes_bypass_guard: Some(true),
            ..Facts::default()
        }
    }

    fn check<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks.iter().find(|c| c.id == id).expect("check exists")
    }

    #[test]
    fn good_machine_passes() {
        assert!(all_required_pass(&evaluate(&good())));
    }

    #[test]
    fn heap_that_only_opens_at_1gib_fails() {
        let mut f = good();
        f.in_place_14gib = Some(false);
        let checks = evaluate(&f);
        assert!(!all_required_pass(&checks));
        assert!(!check(&checks, "in_place_heap").passed);
    }

    #[test]
    fn too_few_address_bits_fails() {
        let mut f = good();
        f.va_bits_per_resource = TITLE_SPACE_BITS - 1;
        assert!(!check(&evaluate(&f), "va_bits").passed);
        assert!(!all_required_pass(&evaluate(&f)));
    }

    #[test]
    fn wrong_data_from_in_place_reads_fails() {
        let mut f = good();
        f.in_place_reads_match = Some(false);
        assert!(!all_required_pass(&evaluate(&f)));
    }

    #[test]
    fn missing_guard_measurement_fails() {
        let mut f = good();
        f.guard = None;
        assert!(!all_required_pass(&evaluate(&f)));
    }

    #[test]
    fn no_shader_writes_in_place_is_reported_but_not_fatal() {
        let mut f = good();
        f.in_place_uav = Some(false);
        let checks = evaluate(&f);
        assert!(all_required_pass(&checks));
        assert!(!check(&checks, "in_place_uav").passed);
    }

    #[test]
    fn report_serialises_snake_case_places() {
        let mut f = good();
        f.gpu_copy.push(GpuCopy { from: Place::InPlaceRam, to: Place::Video, gib_s: 9.5 });
        let json = serde_json::to_string(&build(f)).unwrap();
        assert!(json.contains("\"in_place_ram\""));
        assert!(json.contains("\"pass\":true"));
    }
}
```

Replace `crates/riptide-probe/src/main.rs` with:

```rust
//! Measures whether the target machine supports Riptide's in-place memory design, before
//! anything depends on it. See README.md in this folder.

mod report;

fn main() {}
```

- [ ] **Step 3: Run the tests, confirm they fail**

Run: `cargo test -p riptide-probe`
Expect: compile errors, "cannot find type `Facts` in this scope" and similar.

- [ ] **Step 4: Minimal implementation**

Add above the tests module in `report.rs`:

```rust
//! The probe's findings, and the judgement of them against what Riptide's design needs.

use serde::Serialize;

/// Address bits one buffer must cover to hold the whole title block (16 GiB).
pub const TITLE_SPACE_BITS: u32 = 34;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// RAM in a committed section that is also a D3D12 heap: title memory, used in place.
    InPlaceRam,
    /// Video memory in a default heap.
    Video,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Ordinary cached system memory, the baseline.
    CachedSystem,
    /// A mapped `D3D12_HEAP_TYPE_GPU_UPLOAD` buffer (video memory through Resizable BAR).
    GpuUpload,
}

#[derive(Serialize, Clone, Debug)]
pub struct GpuCopy {
    pub from: Place,
    pub to: Place,
    pub gib_s: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct RandomRead {
    pub source: Place,
    pub ns_per_read: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct GuardCost {
    /// Microseconds from a write to a protected page until the write completes.
    pub fault_us: f64,
    /// Microseconds to write-protect one GiB.
    pub protect_us_per_gib: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Bandwidth {
    pub memory: MemoryKind,
    pub write_gib_s: f64,
    pub read_gib_s: f64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Facts {
    pub adapter: String,
    pub driver_version: String,
    pub dedicated_video_memory_mib: u64,
    pub va_bits_per_resource: u32,
    pub va_bits_per_process: u32,
    /// A 1 GiB section opened as a heap with one buffer, and data crossed both ways. `None`: not tried.
    pub in_place_1gib: Option<bool>,
    /// The same at the title block's size.
    pub in_place_14gib: Option<bool>,
    pub gpu_copy: Vec<GpuCopy>,
    pub gpu_random_read: Vec<RandomRead>,
    /// A shader reading in-place memory computed the same sums as from a video-memory copy.
    pub in_place_reads_match: Option<bool>,
    /// A shader wrote to in-place memory and the CPU read the result.
    pub in_place_uav: Option<bool>,
    pub guard: Option<GuardCost>,
    /// A GPU write to a write-protected page landed without raising a fault.
    pub gpu_writes_bypass_guard: Option<bool>,
    /// The D3D12 runtime answered the `D3D12_OPTIONS16` query at all.
    pub options16_known: bool,
    pub gpu_upload_heap_supported: bool,
    pub cpu_bandwidth: Vec<Bandwidth>,
    pub notes: Vec<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Check {
    pub id: &'static str,
    /// The open question in docs/design/2026-11-27-architecture.md this check answers.
    pub question: &'static str,
    pub required: bool,
    pub passed: bool,
    pub detail: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct Report {
    pub facts: Facts,
    pub checks: Vec<Check>,
    pub pass: bool,
}

pub fn evaluate(f: &Facts) -> Vec<Check> {
    vec![
        Check {
            id: "in_place_heap",
            question: "Q1",
            required: true,
            passed: f.in_place_14gib == Some(true),
            detail: format!("1 GiB: {:?}, 14 GiB: {:?}", f.in_place_1gib, f.in_place_14gib),
        },
        Check {
            id: "va_bits",
            question: "Q1",
            required: true,
            passed: f.va_bits_per_resource >= TITLE_SPACE_BITS,
            detail: format!(
                "per resource: {}, per process: {}, needed: {}",
                f.va_bits_per_resource, f.va_bits_per_process, TITLE_SPACE_BITS
            ),
        },
        Check {
            id: "in_place_reads",
            question: "Q2",
            required: true,
            passed: f.in_place_reads_match == Some(true),
            detail: format!("shader sums from in-place memory match video memory: {:?}", f.in_place_reads_match),
        },
        Check {
            id: "in_place_uav",
            question: "Q3",
            required: false,
            passed: f.in_place_uav == Some(true),
            detail: format!("shader write to in-place memory seen by the CPU: {:?}", f.in_place_uav),
        },
        Check {
            id: "guard",
            question: "Q4",
            required: true,
            passed: f.guard.is_some(),
            detail: match &f.guard {
                Some(g) => format!("{:.2} us per fault, {:.0} us to protect 1 GiB", g.fault_us, g.protect_us_per_gib),
                None => "not measured".to_string(),
            },
        },
        Check {
            id: "gpu_bypasses_guard",
            question: "Q4",
            required: false,
            passed: f.gpu_writes_bypass_guard == Some(true),
            detail: format!("GPU write to a protected page landed without a fault: {:?}", f.gpu_writes_bypass_guard),
        },
    ]
}

pub fn all_required_pass(checks: &[Check]) -> bool {
    checks.iter().filter(|c| c.required).all(|c| c.passed)
}

pub fn build(facts: Facts) -> Report {
    let checks = evaluate(&facts);
    let pass = all_required_pass(&checks);
    Report { facts, checks, pass }
}
```

- [ ] **Step 5: Run the tests, confirm they pass**

Run: `cargo test -p riptide-probe`
Expect: 7 passed. Dead-code warnings for unused items are expected until Task 8 wires them in.

- [ ] **Step 6: Commit**

`git add crates/riptide-probe && git commit -m "Add the probe's report model and requirement checks"`

---

### Task 2: Device, adapter selection and feature queries

**Files:**
- Create: `crates/riptide-probe/src/device.rs`
- Modify: `crates/riptide-probe/src/main.rs` (add `mod device;`)
- Test: `crates/riptide-probe/src/device.rs` (unit tests module)

**Interfaces:**
- Consumes: `report::Facts`.
- Produces:
  - `pub struct AdapterInfo { pub name: String, pub dedicated_video_memory: u64, pub software: bool }`
  - `pub fn pick_adapter(adapters: &[AdapterInfo]) -> Option<usize>`
  - `pub fn format_umd_version(raw: i64) -> String`
  - `pub struct Gpu { pub info: AdapterInfo, pub driver_version: String, pub device: ID3D12Device, pub queue: ID3D12CommandQueue }`
  - `pub fn open(debug: bool) -> windows::core::Result<Gpu>`
  - `pub fn query_features(gpu: &Gpu, facts: &mut Facts)` fills `va_bits_per_resource`, `va_bits_per_process`, `options16_known`, `gpu_upload_heap_supported`, and appends to `notes` on query failure.
  - `pub fn submit(gpu: &Gpu, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> windows::core::Result<()>` records one command list, runs it and waits for it.

- [ ] **Step 1: Write the failing tests**

Create `crates/riptide-probe/src/device.rs` with only the tests module, and add `mod device;` to `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn a(name: &str, mib: u64, software: bool) -> AdapterInfo {
        AdapterInfo { name: name.into(), dedicated_video_memory: mib << 20, software }
    }

    #[test]
    fn picks_largest_hardware_adapter() {
        let list = [a("iGPU", 512, false), a("RX 7900 XTX", 24_560, false), a("Basic Render", 0, true)];
        assert_eq!(pick_adapter(&list), Some(1));
    }

    #[test]
    fn never_picks_software_adapter() {
        assert_eq!(pick_adapter(&[a("Basic Render", 99_999, true)]), None);
    }

    #[test]
    fn formats_driver_version_words() {
        let raw: i64 = (32_i64 << 48) | (21_025 << 16) | 1_010;
        assert_eq!(format_umd_version(raw), "32.0.21025.1010");
    }

    #[test]
    #[ignore = "needs the target machine"]
    fn opens_the_target_gpu() {
        let gpu = open(false).unwrap();
        assert!(gpu.info.name.contains("7900"), "picked {}", gpu.info.name);
        let mut facts = crate::report::Facts::default();
        query_features(&gpu, &mut facts);
        assert!(facts.va_bits_per_resource >= 32, "notes: {:?}", facts.notes);
        submit(&gpu, |_| {}).unwrap();
    }
}
```

- [ ] **Step 2: Run the tests, confirm they fail**

Run: `cargo test -p riptide-probe device`
Expect: compile errors, "cannot find type `AdapterInfo`".

- [ ] **Step 3: Minimal implementation**

Add above the tests module:

```rust
//! The D3D12 device on the target GPU, its feature answers, and one-shot command submission.

use crate::report::Facts;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_1;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[derive(Clone, Debug)]
pub struct AdapterInfo {
    pub name: String,
    pub dedicated_video_memory: u64,
    pub software: bool,
}

/// The hardware adapter with the most dedicated video memory.
pub fn pick_adapter(adapters: &[AdapterInfo]) -> Option<usize> {
    adapters
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.software)
        .max_by_key(|(_, a)| a.dedicated_video_memory)
        .map(|(i, _)| i)
}

/// The user-mode driver version DXGI reports, as four 16-bit words.
pub fn format_umd_version(raw: i64) -> String {
    let w = |shift: u32| (raw >> shift) & 0xFFFF;
    format!("{}.{}.{}.{}", w(48), w(32), w(16), w(0))
}

pub struct Gpu {
    pub info: AdapterInfo,
    pub driver_version: String,
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
}

pub fn open(debug: bool) -> windows::core::Result<Gpu> {
    // SAFETY: plain COM calls with valid out-pointers; every returned interface is owned by
    // a windows-crate smart pointer that releases it.
    unsafe {
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            D3D12GetDebugInterface(&mut layer)?;
            if let Some(layer) = layer {
                layer.EnableDebugLayer();
            }
        }
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let mut adapters = Vec::new();
        let mut infos = Vec::new();
        let mut i = 0;
        while let Ok(adapter) =
            factory.EnumAdapterByGpuPreference::<IDXGIAdapter4>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
        {
            let d = adapter.GetDesc3()?;
            let name = String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string();
            infos.push(AdapterInfo {
                name,
                dedicated_video_memory: d.DedicatedVideoMemory as u64,
                software: (d.Flags.0 & DXGI_ADAPTER_FLAG3_SOFTWARE.0) != 0,
            });
            adapters.push(adapter);
            i += 1;
        }
        let index = pick_adapter(&infos)
            .ok_or_else(|| windows::core::Error::new(DXGI_ERROR_NOT_FOUND, "no hardware adapter"))?;
        let adapter = &adapters[index];
        let driver_version = adapter
            .CheckInterfaceSupport(&IDXGIDevice::IID)
            .map(format_umd_version)
            .unwrap_or_else(|e| format!("unknown ({e})"));
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_1, &mut device)?;
        let device = device.expect("D3D12CreateDevice succeeded without a device");
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        Ok(Gpu { info: infos[index].clone(), driver_version, device, queue })
    }
}

fn feature<T: Default>(gpu: &Gpu, id: D3D12_FEATURE) -> windows::core::Result<T> {
    let mut data = T::default();
    // SAFETY: `data` is a writable `T` and the size passed is exactly `size_of::<T>()`; each
    // caller pairs `id` with its matching D3D12_FEATURE_DATA_* type.
    unsafe {
        gpu.device.CheckFeatureSupport(id, &mut data as *mut T as *mut _, std::mem::size_of::<T>() as u32)?;
    }
    Ok(data)
}

pub fn query_features(gpu: &Gpu, facts: &mut Facts) {
    match feature::<D3D12_FEATURE_DATA_GPU_VIRTUAL_ADDRESS_SUPPORT>(gpu, D3D12_FEATURE_GPU_VIRTUAL_ADDRESS_SUPPORT) {
        Ok(v) => {
            facts.va_bits_per_resource = v.MaxGPUVirtualAddressBitsPerResource;
            facts.va_bits_per_process = v.MaxGPUVirtualAddressBitsPerProcess;
        }
        Err(e) => facts.notes.push(format!("GPU virtual address query failed ({e})")),
    }
    match feature::<D3D12_FEATURE_DATA_D3D12_OPTIONS16>(gpu, D3D12_FEATURE_D3D12_OPTIONS16) {
        Ok(o) => {
            facts.options16_known = true;
            facts.gpu_upload_heap_supported = o.GPUUploadHeapSupported.as_bool();
        }
        Err(e) => facts.notes.push(format!("D3D12_OPTIONS16 query failed ({e}); the runtime may predate it")),
    }
}

pub fn submit(gpu: &Gpu, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> windows::core::Result<()> {
    // SAFETY: the allocator and list outlive the submission because we wait on the fence
    // before returning; the list is closed before it is executed.
    unsafe {
        let alloc: ID3D12CommandAllocator = gpu.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let list: ID3D12GraphicsCommandList =
            gpu.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None::<&ID3D12PipelineState>)?;
        record(&list);
        list.Close()?;
        gpu.queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>()?)]);
        let fence: ID3D12Fence = gpu.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        gpu.queue.Signal(&fence, 1)?;
        while fence.GetCompletedValue() < 1 {
            gpu.device.GetDeviceRemovedReason()?;
            std::thread::yield_now();
        }
        gpu.device.GetDeviceRemovedReason()
    }
}
```

`GetCompletedValue` returns `u64::MAX` once the device is removed, so the loop ends, and the final `GetDeviceRemovedReason` turns that into an error.

- [ ] **Step 4: Run the tests, confirm they pass**

Run: `cargo test -p riptide-probe device`
Expect: 3 passed, 1 ignored.

Then on the target machine: `cargo test -p riptide-probe --release device -- --ignored`
Expect: `opens_the_target_gpu` passes. If it picks another adapter, the assertion names it.

- [ ] **Step 5: Commit**

`git add crates/riptide-probe && git commit -m "Open the D3D12 device on the target GPU and query its features"`

---

### Task 3: The in-place heap (Q1)

A committed section of RAM is mapped for the CPU and opened as a D3D12 heap, with one buffer over all of it. The check proves data crosses both ways: the CPU writes the last 64 KiB and the GPU copies it out; the GPU copies into the first 64 KiB and the CPU reads it.

**Files:**
- Create: `crates/riptide-probe/src/buffers.rs`
- Create: `crates/riptide-probe/src/inplace.rs`
- Modify: `crates/riptide-probe/src/main.rs` (add `mod buffers; mod inplace;`)
- Test: unit tests modules in both files

**Interfaces:**
- Consumes: `device::{Gpu, submit}`, `report::Facts`.
- Produces, in `buffers.rs`:
  - `pub const TILE: u64 = 65_536; pub const MIB: u64 = 1 << 20; pub const GIB: u64 = 1 << 30;`
  - `pub fn pattern(salt: u8, len: usize) -> Vec<u8>`
  - `pub fn buffer_desc(bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC`
  - `pub fn committed(gpu: &Gpu, heap: D3D12_HEAP_TYPE, bytes: u64, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> windows::core::Result<ID3D12Resource>`
  - `pub fn map(resource: &ID3D12Resource) -> windows::core::Result<*mut u8>`
  - `pub fn upload(gpu: &Gpu, dst: &ID3D12Resource, offset: u64, data: &[u8]) -> windows::core::Result<()>`
  - `pub fn read_back(gpu: &Gpu, src: &ID3D12Resource, offset: u64, bytes: u64) -> windows::core::Result<Vec<u8>>`
- Produces, in `inplace.rs`:
  - `pub struct InPlace { pub view: *mut u8, pub bytes: u64, pub buffer: ID3D12Resource, .. }`
  - `pub fn open(gpu: &Gpu, bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> windows::core::Result<InPlace>` (`flags` is combined with `ALLOW_CROSS_ADAPTER`)
  - `impl InPlace { pub fn slice(&self, offset: u64, len: u64) -> &mut [u8] }`
  - `pub fn round_trip(gpu: &Gpu, mem: &InPlace) -> windows::core::Result<bool>`
  - `pub fn check_in_place(gpu: &Gpu, facts: &mut Facts)` sets `in_place_1gib` and `in_place_14gib`.

- [ ] **Step 1: Write the failing tests**

Create `crates/riptide-probe/src/buffers.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_differ_by_salt_and_position() {
        let a = pattern(1, 4096);
        let b = pattern(2, 4096);
        assert_eq!(a.len(), 4096);
        assert_ne!(a, b);
        assert!(a.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn buffer_desc_is_a_row_major_buffer() {
        let d = buffer_desc(TILE, D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER);
        assert_eq!(d.Dimension, D3D12_RESOURCE_DIMENSION_BUFFER);
        assert_eq!(d.Layout, D3D12_TEXTURE_LAYOUT_ROW_MAJOR);
        assert_eq!(d.Width, TILE);
        assert_eq!(d.Flags, D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER);
    }
}
```

Create `crates/riptide-probe/src/inplace.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs the target machine"]
    fn title_sized_block_opens_in_place() {
        let gpu = crate::device::open(false).unwrap();
        let mut facts = Facts::default();
        check_in_place(&gpu, &mut facts);
        assert_eq!(facts.in_place_1gib, Some(true), "notes: {:?}", facts.notes);
        assert_eq!(facts.in_place_14gib, Some(true), "notes: {:?}", facts.notes);
    }
}
```

Add `mod buffers; mod inplace;` to `main.rs`.

- [ ] **Step 2: Run the tests, confirm they fail**

Run: `cargo test -p riptide-probe buffers`
Expect: compile errors, "cannot find function `pattern`".

- [ ] **Step 3: Implement `buffers.rs`**

Add above its tests module:

```rust
//! Buffers of each kind, and moving bytes between the CPU and the GPU for checks.

use crate::device::{Gpu, submit};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};

pub const TILE: u64 = 65_536;
pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

/// Bytes that differ by position and by `salt`, to tell regions and runs apart.
pub fn pattern(salt: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8 ^ salt).collect()
}

pub fn buffer_desc(bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
    }
}

pub fn committed(
    gpu: &Gpu,
    heap: D3D12_HEAP_TYPE,
    bytes: u64,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let props = D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() };
    let mut r: Option<ID3D12Resource> = None;
    // SAFETY: valid descriptions and out-pointer.
    unsafe {
        gpu.device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &buffer_desc(bytes, flags), state, None, &mut r)?
    };
    Ok(r.expect("CreateCommittedResource succeeded without a resource"))
}

/// Maps subresource 0 and returns its CPU address. Resources stay mapped until released.
pub fn map(resource: &ID3D12Resource) -> windows::core::Result<*mut u8> {
    let mut p = std::ptr::null_mut();
    // SAFETY: buffers have one subresource; `p` is a valid out-pointer.
    unsafe { resource.Map(0, None, Some(&mut p))? };
    Ok(p.cast())
}

/// Has the GPU copy `data` into `dst` at `offset`.
pub fn upload(gpu: &Gpu, dst: &ID3D12Resource, offset: u64, data: &[u8]) -> windows::core::Result<()> {
    let len = data.len() as u64;
    let staging = committed(gpu, D3D12_HEAP_TYPE_UPLOAD, len, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ)?;
    let p = map(&staging)?;
    // SAFETY: `staging` maps `len` bytes and `data` holds exactly `len` bytes.
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len()) };
    submit(gpu, |list| {
        // SAFETY: the caller guarantees `offset + len` lies inside `dst`; `staging` is `len` bytes.
        unsafe { list.CopyBufferRegion(dst, offset, &staging, 0, len) }
    })
}

/// Has the GPU copy `bytes` bytes of `src` at `offset` out to the CPU.
pub fn read_back(gpu: &Gpu, src: &ID3D12Resource, offset: u64, bytes: u64) -> windows::core::Result<Vec<u8>> {
    let out = committed(gpu, D3D12_HEAP_TYPE_READBACK, bytes, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST)?;
    submit(gpu, |list| {
        // SAFETY: the caller guarantees `offset + bytes` lies inside `src`; `out` is `bytes` bytes.
        unsafe { list.CopyBufferRegion(&out, 0, src, offset, bytes) }
    })?;
    let p = map(&out)?;
    // SAFETY: `out` maps `bytes` bytes, all written by the finished copy above.
    Ok(unsafe { std::slice::from_raw_parts(p, bytes as usize) }.to_vec())
}
```

Run: `cargo test -p riptide-probe buffers`
Expect: 2 passed.

- [ ] **Step 4: Implement `inplace.rs`**

Add above its tests module:

```rust
//! Title memory as the design has it: a committed section of RAM, mapped for the CPU and
//! opened as a D3D12 heap with one buffer over all of it (open question Q1).

use crate::buffers::{GIB, TILE, buffer_desc, pattern, read_back, upload};
use crate::device::Gpu;
use crate::report::Facts;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Memory::*;
use windows::core::{Interface, PCWSTR};

/// The section and its view. Unmaps and closes on drop, so every error path cleans up.
struct Section {
    handle: HANDLE,
    view: *mut u8,
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: `view` and `handle` came from MapViewOfFile and CreateFileMappingW and are
        // released exactly once, here. A heap opened on the section keeps the memory alive.
        unsafe {
            if !self.view.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            }
            let _ = CloseHandle(self.handle);
        }
    }
}

pub struct InPlace {
    pub view: *mut u8,
    pub bytes: u64,
    pub buffer: ID3D12Resource,
    _heap: ID3D12Heap,
    _section: Section,
}

impl InPlace {
    /// The CPU's view of `len` bytes at `offset`.
    #[allow(clippy::mut_from_ref)]
    pub fn slice(&self, offset: u64, len: u64) -> &mut [u8] {
        assert!(offset + len <= self.bytes);
        // SAFETY: the view maps `bytes` readable and writable bytes for as long as `self`
        // lives; the probe is single-threaded and never holds two slices over one range.
        unsafe { std::slice::from_raw_parts_mut(self.view.add(offset as usize), len as usize) }
    }
}

pub fn open(gpu: &Gpu, bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> windows::core::Result<InPlace> {
    // SAFETY: plain Win32 and COM calls with valid arguments; ownership of the handle and
    // the view passes to `Section` at once.
    unsafe {
        let handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE | SEC_COMMIT,
            (bytes >> 32) as u32,
            bytes as u32,
            PCWSTR::null(),
        )?;
        let mut section = Section { handle, view: std::ptr::null_mut() };
        section.view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize).Value.cast();
        if section.view.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        let device: ID3D12Device3 = gpu.device.cast()?;
        let heap: ID3D12Heap = device.OpenExistingHeapFromFileMapping(handle)?;
        let desc = buffer_desc(bytes, flags | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER);
        let mut buffer: Option<ID3D12Resource> = None;
        gpu.device.CreatePlacedResource(&heap, 0, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut buffer)?;
        Ok(InPlace {
            view: section.view,
            bytes,
            buffer: buffer.expect("CreatePlacedResource succeeded without a resource"),
            _heap: heap,
            _section: section,
        })
    }
}

/// CPU writes the last tile and the GPU copies it out; the GPU copies into the first tile
/// and the CPU reads it. True when both arrive intact.
pub fn round_trip(gpu: &Gpu, mem: &InPlace) -> windows::core::Result<bool> {
    let last = mem.bytes - TILE;
    let from_cpu = pattern(0x11, TILE as usize);
    mem.slice(last, TILE).copy_from_slice(&from_cpu);
    let seen_by_gpu = read_back(gpu, &mem.buffer, last, TILE)?;

    let from_gpu = pattern(0x22, TILE as usize);
    upload(gpu, &mem.buffer, 0, &from_gpu)?;
    let seen_by_cpu = mem.slice(0, TILE).to_vec();

    Ok(seen_by_gpu == from_cpu && seen_by_cpu == from_gpu)
}

fn try_size(gpu: &Gpu, bytes: u64, facts: &mut Facts) -> bool {
    match open(gpu, bytes, D3D12_RESOURCE_FLAG_NONE).and_then(|mem| round_trip(gpu, &mem)) {
        Ok(true) => true,
        Ok(false) => {
            facts.notes.push(format!("in-place {} GiB: data did not arrive intact", bytes / GIB));
            false
        }
        Err(e) => {
            facts.notes.push(format!("in-place {} GiB: {e}", bytes / GIB));
            false
        }
    }
}

pub fn check_in_place(gpu: &Gpu, facts: &mut Facts) {
    facts.in_place_1gib = Some(try_size(gpu, GIB, facts));
    facts.in_place_14gib = Some(try_size(gpu, 14 * GIB, facts));
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p riptide-probe`
Expect: 12 passed (7 + 3 + 2), 2 ignored.

Then on the target machine: `cargo test -p riptide-probe --release inplace -- --ignored`
Expect: `title_sized_block_opens_in_place` passes. A failure message carries the notes. "1 GiB true, 14 GiB false" means the size is the limit; write the error down for Task 8, because the design then needs several heaps. Both false means Q1 is answered "no".

- [ ] **Step 6: Commit**

`git add crates/riptide-probe && git commit -m "Open a committed section as an in-place D3D12 heap and verify both directions"`

---

### Task 4: GPU copy throughput (Q2, bulk)

Times the GPU copying 256 MiB between in-place memory and video memory, in each direction, against a video-to-video baseline. Best of five wall-clock runs after one warm-up.

**Files:**
- Create: `crates/riptide-probe/src/timing.rs`
- Create: `crates/riptide-probe/src/gpucopy.rs`
- Modify: `crates/riptide-probe/src/main.rs` (add `mod timing; mod gpucopy;`)
- Test: unit tests modules in both files

**Interfaces:**
- Consumes: `device::{Gpu, submit}`, `buffers::{MIB, GIB, committed, pattern}`, `inplace::{InPlace, open}`, `report::{Facts, GpuCopy, Place}`.
- Produces:
  - `timing::gib_per_s(bytes: u64, elapsed: Duration) -> f64`
  - `timing::best(samples: &[Duration]) -> Duration`
  - `timing::RUNS: usize = 5`
  - `gpucopy::check_gpu_copy(gpu: &Gpu, mem: &InPlace, facts: &mut Facts)` pushes three `GpuCopy` entries: in-place to video, video to video, video to in-place. `mem` must be at least 256 MiB.

- [ ] **Step 1: Write the failing tests**

Create `crates/riptide-probe/src/timing.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_gib_in_half_a_second_is_two_gib_per_second() {
        assert_eq!(gib_per_s(1 << 30, Duration::from_millis(500)), 2.0);
    }

    #[test]
    fn best_is_the_shortest_sample() {
        let s = [Duration::from_millis(9), Duration::from_millis(4), Duration::from_millis(7)];
        assert_eq!(best(&s), Duration::from_millis(4));
    }
}
```

Create `crates/riptide-probe/src/gpucopy.rs` with only:

```rust
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
```

Add `mod timing; mod gpucopy;` to `main.rs`.

- [ ] **Step 2: Run the tests, confirm they fail**

Run: `cargo test -p riptide-probe timing`
Expect: compile error, "cannot find function `gib_per_s`".

- [ ] **Step 3: Minimal implementation**

Add above the tests module in `timing.rs`:

```rust
//! Arithmetic shared by the timed checks.

use std::time::Duration;

/// Timed runs kept per measurement, after one warm-up run that is thrown away.
pub const RUNS: usize = 5;

pub fn gib_per_s(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / (1u64 << 30) as f64 / elapsed.as_secs_f64()
}

pub fn best(samples: &[Duration]) -> Duration {
    *samples.iter().min().expect("at least one sample")
}
```

Add above the tests module in `gpucopy.rs`:

```rust
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p riptide-probe`
Expect: 14 passed, 3 ignored.

Then on the target machine: `cargo test -p riptide-probe --release gpucopy -- --ignored --nocapture`
Expect: three entries printed. The numbers are the answer to Q2's bulk half; record them, do not judge them.

- [ ] **Step 5: Commit**

`git add crates/riptide-probe && git commit -m "Time GPU copies between in-place memory and video memory"`

---

### Task 5: Scattered shader reads and an in-place shader write (Q2, Q3)

A compute shader does 256 pseudo-random 4-byte reads per thread over 256 MiB, on 65,536 threads, and stores each thread's sum. It runs once reading in-place memory and once reading a video-memory copy of the same bytes. Equal sums prove shaders read in-place memory correctly; the times give nanoseconds per read. A third run stores its sums into in-place memory, and the CPU reads them, which answers Q3.

**Files:**
- Create: `crates/riptide-probe/src/gpuread.rs`
- Modify: `crates/riptide-probe/src/main.rs` (add `mod gpuread;`)
- Test: `crates/riptide-probe/src/gpuread.rs` (unit tests module)

**Interfaces:**
- Consumes: `device::{Gpu, submit}`, `buffers::{MIB, committed, read_back, pattern}`, `inplace::InPlace`, `report::{Facts, Place, RandomRead}`, `timing::{RUNS, best}`.
- Produces:
  - `pub fn compile() -> Result<Vec<u8>, String>` compiles the shader and returns its bytecode. Needs no GPU.
  - `pub fn check_gpu_read(gpu: &Gpu, mem: &InPlace, facts: &mut Facts)` pushes two `RandomRead` entries and sets `in_place_reads_match` and `in_place_uav`. `mem` must be at least 512 MiB and created with `D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS`; if `open` refused that flag, the caller passes a buffer without it and this function records `in_place_uav = Some(false)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/riptide-probe/src/gpuread.rs` with only the tests module, and add `mod gpuread;` to `main.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests, confirm they fail**

Run: `cargo test -p riptide-probe gpuread`
Expect: compile error, "cannot find function `compile`".

- [ ] **Step 3: Minimal implementation**

Add above the tests module:

```rust
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
```

A failed Q3 dispatch can remove the device. `main` (Task 8) therefore runs this check after the copy timings and opens a fresh device afterwards if `in_place_uav` is `Some(false)`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p riptide-probe`
Expect: 15 passed, 4 ignored. `shader_compiles` needs `d3dcompiler_47.dll`, which ships with Windows.

Then on the target machine: `cargo test -p riptide-probe --release gpuread -- --ignored --nocapture`
Expect: the test passes and prints two `RandomRead` entries and the Q3 result. If `open` with `ALLOW_UNORDERED_ACCESS` fails in the test's setup, that is itself the answer to Q3 ("no"); change the test's flag to `D3D12_RESOURCE_FLAG_NONE`, rerun, and write down the error for Task 8.

- [ ] **Step 5: Commit**

`git add crates/riptide-probe && git commit -m "Time scattered shader reads in place and test shader writes to title memory"`

---

### Task 6: Write-protect faults (Q4)

Promotion and texture conversion both rely on write-protecting pages and catching the first write. This task measures what that costs, and checks that a GPU write to a protected page lands without a fault (the design's texture write-back depends on it).

**Files:**
- Create: `crates/riptide-probe/src/guard.rs`
- Modify: `crates/riptide-probe/src/main.rs` (add `mod guard;`)
- Test: `crates/riptide-probe/src/guard.rs` (unit tests module)

**Interfaces:**
- Consumes: `device::Gpu`, `buffers::{MIB, GIB, TILE, pattern, upload}`, `inplace::InPlace`, `report::{Facts, GuardCost}`.
- Produces:
  - `pub fn measure(base: *mut u8, len: usize) -> Result<GuardCost, String>` protects `len` bytes at `base` (page-aligned, readable and writable), writes one byte to each 4 KiB page, and returns the cost. Leaves the range writable.
  - `pub fn check_guard(gpu: &Gpu, mem: &InPlace, facts: &mut Facts)` sets `guard` and `gpu_writes_bypass_guard`.

- [ ] **Step 1: Write the failing tests**

Create `crates/riptide-probe/src/guard.rs` with only the tests module, and add `mod guard;` to `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

    #[test]
    fn every_protected_page_faults_once_and_stays_writable() {
        let len = 256 * PAGE;
        // SAFETY: a fresh private allocation, released at the end of the test.
        let base = unsafe { VirtualAlloc(None, len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }.cast::<u8>();
        assert!(!base.is_null());
        let cost = measure(base, len).unwrap();
        assert!(cost.fault_us > 0.0 && cost.protect_us_per_gib > 0.0);
        // SAFETY: the range is writable again after `measure`; a second write must not fault.
        unsafe { base.write_volatile(7) };
        // SAFETY: releasing the allocation made above.
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) }.unwrap();
    }

    #[test]
    #[ignore = "needs the target machine"]
    fn guards_work_on_in_place_memory() {
        let gpu = crate::device::open(false).unwrap();
        let mem = crate::inplace::open(&gpu, GIB, windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_FLAG_NONE).unwrap();
        let mut facts = Facts::default();
        check_guard(&gpu, &mem, &mut facts);
        assert!(facts.guard.is_some(), "notes: {:?}", facts.notes);
        eprintln!("{:?} gpu bypass: {:?}", facts.guard, facts.gpu_writes_bypass_guard);
    }
}
```

- [ ] **Step 2: Run the tests, confirm they fail**

Run: `cargo test -p riptide-probe guard`
Expect: compile error, "cannot find function `measure`".

- [ ] **Step 3: Minimal implementation**

Add above the tests module:

```rust
//! What catching the first write to protected title memory costs (open question Q4).

use crate::buffers::{GIB, MIB, TILE, pattern, upload};
use crate::device::Gpu;
use crate::inplace::InPlace;
use crate::report::{Facts, GuardCost};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, RemoveVectoredExceptionHandler,
};
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE, VirtualProtect};

pub const PAGE: usize = 4096;
const EXCEPTION_ACCESS_VIOLATION: i32 = 0xC000_0005_u32 as i32;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

static GUARD_BASE: AtomicUsize = AtomicUsize::new(0);
static GUARD_LEN: AtomicUsize = AtomicUsize::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// Unprotects the page of a write fault inside the guarded range and retries the write.
unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the OS passes valid exception pointers to a vectored handler.
    let record = unsafe { &*(*info).ExceptionRecord };
    // ExceptionInformation[0] == 1 means a write; [1] is the address written.
    if record.ExceptionCode.0 != EXCEPTION_ACCESS_VIOLATION || record.ExceptionInformation[0] != 1 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let address = record.ExceptionInformation[1];
    let base = GUARD_BASE.load(Relaxed);
    if address < base || address >= base + GUARD_LEN.load(Relaxed) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the page lies inside the guarded range, which the caller of `measure` owns.
    if unsafe { VirtualProtect((address & !(PAGE - 1)) as *const c_void, PAGE, PAGE_READWRITE, &mut old) }.is_err() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    FAULTS.fetch_add(1, Relaxed);
    EXCEPTION_CONTINUE_EXECUTION
}

pub fn measure(base: *mut u8, len: usize) -> Result<GuardCost, String> {
    assert!((base as usize).is_multiple_of(PAGE) && len.is_multiple_of(PAGE) && len > 0);
    let pages = len / PAGE;
    GUARD_BASE.store(base as usize, Relaxed);
    GUARD_LEN.store(len, Relaxed);
    FAULTS.store(0, Relaxed);
    // SAFETY: `on_fault` only acts on write faults inside the guarded range.
    let handler = unsafe { AddVectoredExceptionHandler(1, Some(on_fault)) };
    if handler.is_null() {
        return Err("AddVectoredExceptionHandler failed".into());
    }

    let mut old = PAGE_PROTECTION_FLAGS(0);
    let t = Instant::now();
    // SAFETY: the caller guarantees `len` committed bytes at `base`.
    let protected = unsafe { VirtualProtect(base.cast(), len, PAGE_READONLY, &mut old) };
    let protect = t.elapsed();

    let mut writes = std::time::Duration::ZERO;
    if protected.is_ok() {
        let t = Instant::now();
        for page in 0..pages {
            // SAFETY: inside the range; the fault handler makes the page writable and the
            // write is retried.
            unsafe { base.add(page * PAGE).write_volatile(1) };
        }
        writes = t.elapsed();
    }

    // SAFETY: `handler` is the value AddVectoredExceptionHandler returned.
    unsafe { RemoveVectoredExceptionHandler(handler) };
    GUARD_LEN.store(0, Relaxed);
    protected.map_err(|e| format!("VirtualProtect: {e}"))?;
    let faults = FAULTS.load(Relaxed);
    if faults != pages as u64 {
        return Err(format!("{faults} faults for {pages} protected pages"));
    }
    Ok(GuardCost {
        fault_us: writes.as_secs_f64() * 1e6 / pages as f64,
        protect_us_per_gib: protect.as_secs_f64() * 1e6 * (GIB as f64 / len as f64),
    })
}

pub fn check_guard(gpu: &Gpu, mem: &InPlace, facts: &mut Facts) {
    let len = 64 * MIB;
    assert!(mem.bytes >= len);
    mem.slice(0, len).fill(0);
    match measure(mem.view, len as usize) {
        Ok(cost) => facts.guard = Some(cost),
        Err(e) => {
            facts.notes.push(format!("guard on in-place memory: {e}"));
            return;
        }
    }

    // Does a GPU write to a protected page land, and without a fault?
    let data = pattern(0x55, TILE as usize);
    let mut old = PAGE_PROTECTION_FLAGS(0);
    FAULTS.store(0, Relaxed);
    // SAFETY: the first tile of the view is committed and owned by `mem`.
    if let Err(e) = unsafe { VirtualProtect(mem.view.cast(), TILE as usize, PAGE_READONLY, &mut old) } {
        facts.notes.push(format!("protect before GPU write: {e}"));
        return;
    }
    let wrote = upload(gpu, &mem.buffer, 0, &data);
    let landed = mem.slice(0, TILE) == data.as_slice();
    // SAFETY: restoring the protection changed above.
    let _ = unsafe { VirtualProtect(mem.view.cast(), TILE as usize, PAGE_READWRITE, &mut old) };
    match wrote {
        Ok(()) => facts.gpu_writes_bypass_guard = Some(landed && FAULTS.load(Relaxed) == 0),
        Err(e) => {
            facts.gpu_writes_bypass_guard = Some(false);
            facts.notes.push(format!("GPU write to a protected page: {e}"));
        }
    }
}
```

`measure` uses process-wide statics, so two guard measurements must not run at once. The unit test and the hardware test are the only callers in tests; run this module's tests with `--test-threads=1`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p riptide-probe guard -- --test-threads=1`
Expect: 1 passed, 1 ignored.

Run: `cargo test -p riptide-probe`
Expect: 16 passed, 5 ignored.

Then on the target machine: `cargo test -p riptide-probe --release guard -- --ignored --nocapture --test-threads=1`
Expect: the test passes and prints the cost and whether GPU writes bypass the guard.

- [ ] **Step 5: Commit**

`git add crates/riptide-probe && git commit -m "Measure write-protect fault cost on in-place memory"`

---

### Task 7: CPU bandwidth to GPU upload heap memory (Q5)

The design rejected pointing title memory into video memory because of one published number: about 30 MB/s for CPU reads. This task measures it on the target machine. It needs Smart Access Memory on and a D3D12 runtime that knows `D3D12_OPTIONS16`. If either is missing, the report records cached-memory bandwidth only and a note, and Q5 stays open; adding the Agility SDK to get the number is a follow-up outside this plan.

**Files:**
- Create: `crates/riptide-probe/src/bandwidth.rs`
- Modify: `crates/riptide-probe/src/main.rs` (add `mod bandwidth;`)
- Test: `crates/riptide-probe/src/bandwidth.rs` (unit tests module)

**Interfaces:**
- Consumes: `device::Gpu`, `buffers::{MIB, committed, map}`, `report::{Facts, Bandwidth, MemoryKind}`, `timing::{RUNS, best, gib_per_s}`.
- Produces:
  - `pub fn measure(dst: *mut u8, len: usize) -> (f64, f64)` returns `(write_gib_s, read_gib_s)`.
  - `pub fn check_bandwidth(gpu: &Gpu, facts: &mut Facts)` pushes a `CachedSystem` entry always, and a `GpuUpload` entry when `facts.gpu_upload_heap_supported` is true. Call it after `device::query_features`.

- [ ] **Step 1: Write the failing tests**

Create `crates/riptide-probe/src/bandwidth.rs` with only the tests module, and add `mod bandwidth;` to `main.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests, confirm they fail**

Run: `cargo test -p riptide-probe bandwidth`
Expect: compile error, "cannot find function `measure`".

- [ ] **Step 3: Minimal implementation**

Add above the tests module:

```rust
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
```

`_buffer` keeps the resource alive while `measure` uses its mapping. At 30 MB/s, six read passes over 32 MiB take about 7 seconds.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p riptide-probe`
Expect: 17 passed, 6 ignored.

Then on the target machine: `cargo test -p riptide-probe --release bandwidth -- --ignored --nocapture`
Expect: one or two entries. Record the numbers and any note.

- [ ] **Step 5: Commit**

`git add crates/riptide-probe && git commit -m "Measure CPU bandwidth to cached and GPU upload memory"`

---

### Task 8: Wire the probe together, run it and record the answers

**Files:**
- Modify: `crates/riptide-probe/src/main.rs`
- Modify: `crates/riptide-probe/README.md`
- Modify: `docs/measurements.md`
- Modify: `docs/design/2026-11-27-architecture.md` (answers to Q1 to Q5)

**Interfaces:**
- Consumes: `device::{open, query_features}`, `inplace::{open, check_in_place}`, `gpucopy::check_gpu_copy`, `gpuread::check_gpu_read`, `guard::check_guard`, `bandwidth::check_bandwidth`, `report::{Facts, build}`, `buffers::GIB`.
- Produces: `riptide-probe [--debug]`, which prints a `Report` as pretty JSON on stdout. Exit codes: 0 = all required checks passed, 1 = a required check failed, 2 = no GPU could be opened.

- [ ] **Step 1: Write `main`**

Replace `crates/riptide-probe/src/main.rs` with:

```rust
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
use windows::Win32::Graphics::Direct3D12::{D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_FLAG_NONE};

/// A 1 GiB in-place block for the timed checks, with shader writes allowed when the driver
/// takes that flag.
fn work_block(gpu: &device::Gpu, facts: &mut report::Facts) -> Option<inplace::InPlace> {
    match inplace::open(gpu, buffers::GIB, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS) {
        Ok(mem) => Some(mem),
        Err(e) => {
            facts.notes.push(format!("in-place buffer with unordered access refused: {e}"));
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
                Err(e) => facts.notes.push(format!("device lost and not reopened: {e}")),
            }
        }
    }
    bandwidth::check_bandwidth(&gpu, &mut facts);

    let report = report::build(facts);
    println!("{}", serde_json::to_string_pretty(&report).expect("report serialises"));
    if report.pass { ExitCode::SUCCESS } else { ExitCode::from(1) }
}
```

The shader-read check runs last among the in-place checks because its Q3 step is the one most likely to upset the driver.

- [ ] **Step 2: Build with no warnings**

Run: `cargo clippy -p riptide-probe --all-targets -- -D warnings`
Expect: no output and exit code 0. Fix any warning before continuing.

Run: `cargo test -p riptide-probe -- --test-threads=1`
Expect: 17 passed, 6 ignored.

- [ ] **Step 3: Document running it**

Add this section to `crates/riptide-probe/README.md`:

```markdown
## Running it

    cargo build -p riptide-probe --release
    target/release/riptide-probe.exe > local/probe/report.json

Add `--debug` to turn on the D3D12 debug layer (needs Windows' Graphics Tools optional
feature); its messages explain a refused call.

Exit code 0: every required check passed. 1: a required check failed. 2: the probe could not
open the GPU at all. The probe reserves 14 GiB against the page file for a moment.
```

- [ ] **Step 4: Run the probe on the target machine**

```powershell
cargo build -p riptide-probe --release
New-Item -ItemType Directory -Force local/probe | Out-Null
target/release/riptide-probe.exe > local/probe/2026-11-27.json; $LASTEXITCODE
```

Expect: a JSON report and an exit code of 0 or 1. If a check failed without a clear note, run again with `--debug` and keep stderr: `target/release/riptide-probe.exe --debug 2> local/probe/debug.txt`.

- [ ] **Step 5: Record the results**

Add an entry under "Entries" in `docs/measurements.md`, with every angle-bracket field replaced by the value from that run's JSON:

```markdown
### 2026-11-27 · riptide-probe · commit <short hash>

Driver <driver_version>. `riptide-probe` release build.

| Check | Result |
| --- | --- |
| 14 GiB section as one in-place heap (Q1) | <passed / failed, detail> |
| Address bits per resource (Q1) | <va_bits_per_resource> |
| GPU copy, in-place to video (Q2) | <gib_s> GiB/s |
| GPU copy, video to video (Q2) | <gib_s> GiB/s |
| GPU copy, video to in-place (Q2) | <gib_s> GiB/s |
| Scattered shader read, in-place (Q2) | <ns_per_read> ns |
| Scattered shader read, video (Q2) | <ns_per_read> ns |
| Shader reads in place are correct (Q2) | <yes / no> |
| Shader write to in-place memory (Q3) | <yes / no> |
| Write fault (Q4) | <fault_us> us; protecting 1 GiB takes <protect_us_per_gib> us |
| GPU write passes a guard (Q4) | <yes / no> |
| CPU write / read, cached memory (Q5) | <w> / <r> GiB/s |
| CPU write / read, GPU upload memory (Q5) | <w> / <r> GiB/s, or "not measured" with the note |
```

In `docs/design/2026-11-27-architecture.md`, append to each of Q1 to Q5 a line beginning `**Answer (2026-11-27):**` with the result and a link to the measurements entry. If Q1 failed at every size, add a line at the top of the Decisions section: `D1 is blocked by Q1; redesign before piece 2.`

- [ ] **Step 6: Commit**

`git add crates/riptide-probe docs && git commit -m "Run riptide-probe and record answers to Q1 to Q5"`

The raw JSON stays in `local/` and is not committed.

---

## What this plan does not cover

- Q5 on a runtime that does not know `D3D12_OPTIONS16`. Getting the number then needs the D3D12 Agility SDK beside the executable.
- A 4-byte-word shader view over the whole 14 GiB block (Q6). It belongs with `riptide-shader`, piece 5.
- Promotion thresholds (Q9). They are set from this plan's Q2 and Q4 numbers when `riptide-gpu` is designed.
