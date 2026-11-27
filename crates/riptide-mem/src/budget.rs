//! The RAM budget (decision D10): a limit set once at launch, and use held against it.
//!
//! The runtimes this project learned from use 40 to 46 GB on a 32 GB machine, and the frame
//! rate falls to single digits when Windows starts paging. Riptide sets a limit before the
//! title starts and always knows how close it is.

use riptide_config::Settings;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetCurrentProcess;

const MIB: u64 = 1 << 20;

/// What the automatic budget never exceeds: the README's target for the whole process.
pub const AUTOMATIC_LIMIT: u64 = 16 * 1024 * MIB;

/// Use at or above this share of the budget counts as near the limit.
const NEAR_PERCENT: u64 = 90;

/// How use stands against the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    /// Below 90% of the budget.
    Under,
    /// At 90% of the budget or more, and not over it.
    Near,
    /// Over the budget.
    Over,
}

/// The RAM this process uses, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RamUse {
    /// Title memory that is committed. A committed page the title has never touched uses no
    /// RAM yet, so this is the most the title's memory can be using.
    pub title: u64,
    /// Everything else the process has allocated: Riptide's own memory and the driver's.
    /// Title memory is a shared section, so Windows does not count it here.
    pub private: u64,
}

impl RamUse {
    pub fn total(&self) -> u64 {
        self.title + self.private
    }
}

/// The RAM budget for one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamBudget {
    limit: u64,
}

impl RamBudget {
    /// The budget for a setting of `setting_mib`, on a machine with `free_ram` bytes free.
    /// A setting is taken as it is. Without one, the budget is the free RAM, and never
    /// more than [`AUTOMATIC_LIMIT`].
    pub fn new(setting_mib: Option<u32>, free_ram: u64) -> RamBudget {
        RamBudget {
            limit: match setting_mib {
                Some(mib) => u64::from(mib) * MIB,
                None => free_ram.min(AUTOMATIC_LIMIT),
            },
        }
    }

    /// The budget the settings ask for, on this machine as it is now. Call it once, at
    /// launch, before the title has used anything.
    pub fn from_settings(settings: &Settings) -> RamBudget {
        RamBudget::new(settings.performance.ram_budget_mib, free_ram())
    }

    /// The limit, in bytes.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// How `in_use` stands against the limit.
    pub fn pressure(&self, in_use: RamUse) -> Pressure {
        let total = u128::from(in_use.total());
        let limit = u128::from(self.limit);
        if total > limit {
            Pressure::Over
        } else if total * 100 >= limit * u128::from(NEAR_PERCENT) {
            Pressure::Near
        } else {
            Pressure::Under
        }
    }

    /// How many bytes are left under the limit. Zero when over it.
    pub fn headroom(&self, in_use: RamUse) -> u64 {
        self.limit.saturating_sub(in_use.total())
    }
}

/// Free physical memory right now, in bytes. Zero if Windows will not say.
pub fn free_ram() -> u64 {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set, as the call requires.
    match unsafe { GlobalMemoryStatusEx(&mut status) } {
        Ok(()) => status.ullAvailPhys,
        Err(_) => 0,
    }
}

/// What this process has allocated for itself, in bytes: Windows' "private bytes". Zero if
/// Windows will not say.
pub(crate) fn private_bytes() -> u64 {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: `counters` is a valid out-pointer of the size passed. The extended structure
    // starts with the plain one, which is what the call's signature names.
    let read = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    match read {
        Ok(()) => counters.PrivateUsage as u64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    fn used(title: u64, private: u64) -> RamUse {
        RamUse { title, private }
    }

    #[test]
    fn a_setting_is_the_budget_whatever_is_free() {
        assert_eq!(RamBudget::new(Some(12 * 1024), 4 * GIB).limit(), 12 * GIB);
        assert_eq!(RamBudget::new(Some(8 * 1024), 64 * GIB).limit(), 8 * GIB);
    }

    #[test]
    fn the_automatic_budget_is_what_is_free_and_at_most_16_gib() {
        assert_eq!(RamBudget::new(None, 10 * GIB).limit(), 10 * GIB);
        assert_eq!(RamBudget::new(None, 16 * GIB).limit(), 16 * GIB);
        assert_eq!(RamBudget::new(None, 26 * GIB).limit(), 16 * GIB);
    }

    #[test]
    fn the_budget_is_read_from_the_settings() {
        let mut settings = Settings::default();
        let automatic = RamBudget::from_settings(&settings);
        assert!(automatic.limit() > 0 && automatic.limit() <= AUTOMATIC_LIMIT);

        settings.performance.ram_budget_mib = Some(9 * 1024);
        assert_eq!(RamBudget::from_settings(&settings).limit(), 9 * GIB);
    }

    #[test]
    fn use_is_near_the_limit_from_90_percent_and_over_it_past_100() {
        let budget = RamBudget::new(Some(10 * 1024), 0);
        assert_eq!(budget.pressure(used(0, 0)), Pressure::Under);
        assert_eq!(budget.pressure(used(8 * GIB, GIB - 1)), Pressure::Under);
        assert_eq!(budget.pressure(used(8 * GIB, GIB)), Pressure::Near);
        assert_eq!(budget.pressure(used(9 * GIB, GIB)), Pressure::Near);
        assert_eq!(budget.pressure(used(9 * GIB, GIB + 1)), Pressure::Over);
        assert!(Pressure::Under < Pressure::Near && Pressure::Near < Pressure::Over);
    }

    #[test]
    fn headroom_is_what_is_left_and_never_negative() {
        let budget = RamBudget::new(Some(10 * 1024), 0);
        assert_eq!(budget.headroom(used(4 * GIB, 2 * GIB)), 4 * GIB);
        assert_eq!(budget.headroom(used(9 * GIB, 2 * GIB)), 0);
    }

    #[test]
    fn this_process_has_private_bytes_and_this_machine_has_free_ram() {
        assert!(private_bytes() > 0);
        assert!(free_ram() > 0);
    }
}
