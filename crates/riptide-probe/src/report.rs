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
    /// The title block's size as seven 2 GiB heaps open at once, each with data crossing
    /// both ways. Only tried when one heap of that size would not open. `None`: not tried.
    pub in_place_14gib_split: Option<bool>,
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
            detail: format!(
                "1 GiB: {:?}, 14 GiB: {:?}",
                f.in_place_1gib, f.in_place_14gib
            ),
        },
        Check {
            id: "in_place_split",
            question: "Q1",
            required: false,
            passed: f.in_place_14gib == Some(true) || f.in_place_14gib_split == Some(true),
            detail: format!(
                "14 GiB as seven 2 GiB heaps: {:?} (only tried when one heap fails)",
                f.in_place_14gib_split
            ),
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
            detail: format!(
                "shader sums from in-place memory match video memory: {:?}",
                f.in_place_reads_match
            ),
        },
        Check {
            id: "in_place_uav",
            question: "Q3",
            required: false,
            passed: f.in_place_uav == Some(true),
            detail: format!(
                "shader write to in-place memory seen by the CPU: {:?}",
                f.in_place_uav
            ),
        },
        Check {
            id: "guard",
            question: "Q4",
            required: true,
            passed: f.guard.is_some(),
            detail: match &f.guard {
                Some(g) => format!(
                    "{:.2} us per fault, {:.0} us to protect 1 GiB",
                    g.fault_us, g.protect_us_per_gib
                ),
                None => "not measured".to_string(),
            },
        },
        Check {
            id: "gpu_bypasses_guard",
            question: "Q4",
            required: false,
            passed: f.gpu_writes_bypass_guard == Some(true),
            detail: format!(
                "GPU write to a protected page landed without a fault: {:?}",
                f.gpu_writes_bypass_guard
            ),
        },
    ]
}

pub fn all_required_pass(checks: &[Check]) -> bool {
    checks.iter().filter(|c| c.required).all(|c| c.passed)
}

pub fn build(facts: Facts) -> Report {
    let checks = evaluate(&facts);
    let pass = all_required_pass(&checks);
    Report {
        facts,
        checks,
        pass,
    }
}

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
            guard: Some(GuardCost {
                fault_us: 2.0,
                protect_us_per_gib: 900.0,
            }),
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
        f.gpu_copy.push(GpuCopy {
            from: Place::InPlaceRam,
            to: Place::Video,
            gib_s: 9.5,
        });
        let json = serde_json::to_string(&build(f)).unwrap();
        assert!(json.contains("\"in_place_ram\""));
        assert!(json.contains("\"pass\":true"));
    }
}
