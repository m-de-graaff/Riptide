//! Checks, before a title starts, that the machine can carry it, and says why if it cannot.
//!
//! [`gather`] reads facts from Windows; [`judge`] turns them into checks and is pure.

use riptide_config::Settings;

mod windows;
pub use crate::windows::gather;

use std::fmt;

/// The RAM budget used when the setting is automatic, in MiB.
pub const AUTO_RAM_BUDGET_MIB: u64 = 16 * 1024;
/// Page-file room needed to reserve the 14 GiB title block, with 2 GiB to spare, in MiB.
pub const COMMIT_NEEDED_MIB: u64 = 16 * 1024;
/// Less free space than this on the game's drive draws a warning, in MiB.
pub const GAME_DRIVE_FREE_MIB: u64 = 2 * 1024;
/// The graphics driver Riptide was last measured with, from the newest entry in
/// `docs/measurements.md`. Change it when a newer driver is measured.
pub const MEASURED_DRIVER: Option<Driver> = Some(Driver {
    vendor_id: VENDOR_AMD,
    version: [32, 0, 32029, 2010],
});

/// PCI vendor numbers, for naming the make of a graphics card.
const VENDOR_AMD: u32 = 0x1002;

/// A graphics driver: whose card it drives, and its four-part version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Driver {
    /// The card's PCI vendor number. Versions of different makes cannot be compared.
    pub vendor_id: u32,
    pub version: [u16; 4],
}

impl fmt::Display for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.version;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

/// The drive a folder is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drive {
    pub free_mib: u64,
    /// Whether reads pay for moving a head: true for a hard disk, false for an SSD.
    /// `None`: Windows would not say.
    pub seek_penalty: Option<bool>,
}

/// Whether an antivirus product's state, as Windows' Security Center reports it, says its
/// real-time scanning is on. The state is three bytes: product kind, scanner state,
/// definitions state. A scanner state of 0x10 or 0x11 is on; 0x00 and 0x01 are off.
pub fn real_time_scanning(product_state: u32) -> bool {
    (product_state >> 12) & 0xF == 1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warning,
    Blocking,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub id: &'static str,
    pub level: Level,
    pub title: &'static str,
    pub detail: String,
}

/// Facts about this machine, as far as the checks need them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub free_ram_mib: u64,
    /// How much more memory Windows will let processes commit (RAM plus page file).
    pub commit_headroom_mib: u64,
    /// The hardware graphics card and its driver version. `None`: none found.
    pub gpu: Option<String>,
    /// `None`: the power plan could not be read.
    pub power_saver: Option<bool>,
    /// `None`: no game folder is chosen.
    pub game_folder_exists: Option<bool>,
    pub runtime_present: bool,
    /// The driver of the graphics card in `gpu`. `None`: not found.
    pub gpu_driver: Option<Driver>,
    /// The drive the game folder is on. `None`: no folder, or the drive could not be read.
    pub game_drive: Option<Drive>,
    /// The antivirus products whose real-time scanning is on. `None`: Windows could not be
    /// asked.
    pub scanners: Option<Vec<String>>,
    /// Where Riptide keeps its caches, for telling the user what to exclude.
    pub cache_folder: String,
}

fn gib(mib: u64) -> String {
    format!("{:.1} GiB", mib as f64 / 1024.0)
}

pub fn judge(m: &Machine, s: &Settings) -> Vec<Check> {
    judge_with(m, s, MEASURED_DRIVER)
}

/// [`judge`], with the driver Riptide was last measured with given by the caller.
pub fn judge_with(m: &Machine, s: &Settings, measured: Option<Driver>) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut add = |id, level, title, detail: String| {
        checks.push(Check {
            id,
            level,
            title,
            detail,
        })
    };

    match m.game_folder_exists {
        Some(true) => add("game_folder", Level::Ok, "Game folder", "Found.".into()),
        Some(false) => add(
            "game_folder",
            Level::Blocking,
            "Game folder",
            "The chosen folder does not exist.".into(),
        ),
        None => add(
            "game_folder",
            Level::Blocking,
            "Game folder",
            "Choose the game's folder on the Game tab.".into(),
        ),
    }

    let budget = s
        .performance
        .ram_budget_mib
        .map_or(AUTO_RAM_BUDGET_MIB, u64::from);
    let ram = format!("{} free, {} needed.", gib(m.free_ram_mib), gib(budget));
    if m.free_ram_mib >= budget {
        add("free_ram", Level::Ok, "Free RAM", ram);
    } else if m.free_ram_mib >= budget / 2 {
        add(
            "free_ram",
            Level::Warning,
            "Free RAM",
            format!("{ram} Close other programs, or Windows will page to disk."),
        );
    } else {
        add(
            "free_ram",
            Level::Blocking,
            "Free RAM",
            format!("{ram} Close other programs first."),
        );
    }

    let commit = format!(
        "{} available, {} needed.",
        gib(m.commit_headroom_mib),
        gib(COMMIT_NEEDED_MIB)
    );
    if m.commit_headroom_mib >= COMMIT_NEEDED_MIB {
        add("page_file", Level::Ok, "Page file room", commit);
    } else {
        add(
            "page_file",
            Level::Blocking,
            "Page file room",
            format!("{commit} Enlarge the page file in Windows' settings."),
        );
    }

    match &m.gpu {
        Some(name) => add("gpu", Level::Ok, "Graphics card", name.clone()),
        None => add(
            "gpu",
            Level::Blocking,
            "Graphics card",
            "No hardware graphics card was found.".into(),
        ),
    }

    const DRIVER: &str = "Graphics driver";
    match (m.gpu_driver, measured) {
        (_, None) => add(
            "gpu_driver",
            Level::Ok,
            DRIVER,
            "No driver has been measured yet; not checked.".into(),
        ),
        (None, Some(_)) => add(
            "gpu_driver",
            Level::Ok,
            DRIVER,
            "Could not be read; not checked.".into(),
        ),
        (Some(driver), Some(measured)) if driver.vendor_id != measured.vendor_id => add(
            "gpu_driver",
            Level::Ok,
            DRIVER,
            format!(
                "{driver}. Riptide was measured on {} card; not compared.",
                if measured.vendor_id == VENDOR_AMD {
                    "an AMD"
                } else {
                    "another make of"
                }
            ),
        ),
        (Some(driver), Some(measured)) if driver.version < measured.version => add(
            "gpu_driver",
            Level::Warning,
            DRIVER,
            format!(
                "{driver} is older than {measured}, the driver Riptide was last measured \
                 with. Update the graphics driver."
            ),
        ),
        (Some(driver), Some(measured)) => add(
            "gpu_driver",
            Level::Ok,
            DRIVER,
            format!("{driver}; last measured with {measured}."),
        ),
    }

    const DRIVE: &str = "Game's drive";
    match m.game_drive {
        None => add("game_drive", Level::Ok, DRIVE, "No drive to check.".into()),
        Some(drive) => {
            let free = format!("{} free", gib(drive.free_mib));
            let low = drive.free_mib < GAME_DRIVE_FREE_MIB;
            let mut problems = Vec::new();
            if drive.seek_penalty == Some(true) {
                problems.push(
                    "The game is on a hard disk; streaming from it will stutter. Move it to \
                     an SSD."
                        .to_owned(),
                );
            }
            if low {
                problems.push(format!(
                    "Less than {} is free on it.",
                    gib(GAME_DRIVE_FREE_MIB)
                ));
            }
            if problems.is_empty() {
                let kind = match drive.seek_penalty {
                    Some(false) => "on an SSD",
                    _ => "drive type could not be read",
                };
                add("game_drive", Level::Ok, DRIVE, format!("{free}, {kind}."));
            } else {
                add(
                    "game_drive",
                    Level::Warning,
                    DRIVE,
                    format!("{free}. {}", problems.join(" ")),
                );
            }
        }
    }

    const ANTIVIRUS: &str = "Antivirus";
    match m.scanners.as_deref() {
        None => add(
            "antivirus",
            Level::Ok,
            ANTIVIRUS,
            "Windows could not be asked; not checked.".into(),
        ),
        Some([]) => add(
            "antivirus",
            Level::Ok,
            ANTIVIRUS,
            "No real-time scanner is active.".into(),
        ),
        // A note, not a warning. Windows shows a product's exclusions only to
        // administrators, so this could never be seen to be dealt with, and a warning that
        // never clears teaches its reader to pass over warnings.
        Some(scanners) => add(
            "antivirus",
            Level::Ok,
            ANTIVIRUS,
            format!(
                "{} scans files as they are written. If Riptide's caches load slowly, \
                 exclude {} in it. Riptide cannot see whether that is done already.",
                scanners.join(" and "),
                m.cache_folder
            ),
        ),
    }

    match m.power_saver {
        Some(true) => add(
            "power_plan",
            Level::Warning,
            "Power plan",
            "\"Power saver\" is active and will slow the game.".into(),
        ),
        Some(false) => add(
            "power_plan",
            Level::Ok,
            "Power plan",
            "Not on \"Power saver\".".into(),
        ),
        None => add(
            "power_plan",
            Level::Ok,
            "Power plan",
            "Could not be read; not checked.".into(),
        ),
    }

    let problems = s.problems();
    if problems.is_empty() {
        add(
            "settings",
            Level::Ok,
            "Settings",
            "All values are in range.".into(),
        );
    } else {
        add("settings", Level::Blocking, "Settings", problems.join(" "));
    }

    if m.runtime_present {
        add("runtime", Level::Ok, "Riptide runtime", "Installed.".into());
    } else {
        add(
            "runtime",
            Level::Warning,
            "Riptide runtime",
            "Not installed yet: riptide.exe is not beside the launcher.".into(),
        );
    }
    checks
}

/// False when any check is blocking.
pub fn can_play(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.level != Level::Blocking)
}

/// One line for the launcher's status bar, such as "5 OK, 1 warning, 1 blocking".
pub fn summary(checks: &[Check]) -> String {
    let count = |level| checks.iter().filter(|c| c.level == level).count();
    let (ok, warnings, blocking) = (
        count(Level::Ok),
        count(Level::Warning),
        count(Level::Blocking),
    );
    let mut parts = vec![format!("{ok} OK")];
    if warnings > 0 {
        parts.push(format!(
            "{warnings} warning{}",
            if warnings == 1 { "" } else { "s" }
        ));
    }
    if blocking > 0 {
        parts.push(format!("{blocking} blocking"));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Machine {
        Machine {
            free_ram_mib: 24 * 1024,
            commit_headroom_mib: 40 * 1024,
            gpu: Some("AMD Radeon RX 7900 XTX (driver 32.0.32029.2010)".into()),
            power_saver: Some(false),
            game_folder_exists: Some(true),
            runtime_present: true,
            gpu_driver: Some(amd([32, 0, 32029, 2010])),
            game_drive: Some(Drive {
                free_mib: 500 * 1024,
                seek_penalty: Some(false),
            }),
            scanners: Some(Vec::new()),
            cache_folder: r"C:\Users\someone\AppData\Local\Riptide".into(),
        }
    }

    fn amd(version: [u16; 4]) -> Driver {
        Driver {
            vendor_id: VENDOR_AMD,
            version,
        }
    }

    fn check(checks: &[Check], id: &str) -> Check {
        checks
            .iter()
            .find(|c| c.id == id)
            .expect("check exists")
            .clone()
    }

    fn level(checks: &[Check], id: &str) -> Level {
        checks
            .iter()
            .find(|c| c.id == id)
            .expect("check exists")
            .level
    }

    #[test]
    fn good_machine_is_all_ok() {
        let checks = judge(&good(), &Settings::default());
        assert!(checks.iter().all(|c| c.level == Level::Ok), "{checks:#?}");
        assert!(can_play(&checks));
        assert_eq!(summary(&checks), "10 OK");
    }

    #[test]
    fn missing_runtime_warns_but_does_not_block() {
        let checks = judge(
            &Machine {
                runtime_present: false,
                ..good()
            },
            &Settings::default(),
        );
        assert_eq!(level(&checks, "runtime"), Level::Warning);
        assert!(can_play(&checks));
    }

    #[test]
    fn no_game_folder_blocks() {
        for folder in [None, Some(false)] {
            let checks = judge(
                &Machine {
                    game_folder_exists: folder,
                    ..good()
                },
                &Settings::default(),
            );
            assert_eq!(level(&checks, "game_folder"), Level::Blocking);
            assert!(!can_play(&checks));
        }
    }

    #[test]
    fn low_ram_warns_and_very_low_ram_blocks() {
        let judge_ram = |mib| {
            level(
                &judge(
                    &Machine {
                        free_ram_mib: mib,
                        ..good()
                    },
                    &Settings::default(),
                ),
                "free_ram",
            )
        };
        assert_eq!(judge_ram(AUTO_RAM_BUDGET_MIB), Level::Ok);
        assert_eq!(judge_ram(AUTO_RAM_BUDGET_MIB - 1), Level::Warning);
        assert_eq!(judge_ram(AUTO_RAM_BUDGET_MIB / 2), Level::Warning);
        assert_eq!(judge_ram(AUTO_RAM_BUDGET_MIB / 2 - 1), Level::Blocking);
    }

    #[test]
    fn a_custom_ram_budget_moves_the_threshold() {
        let mut s = Settings::default();
        s.performance.ram_budget_mib = Some(12 * 1024);
        let checks = judge(
            &Machine {
                free_ram_mib: 13 * 1024,
                ..good()
            },
            &s,
        );
        assert_eq!(level(&checks, "free_ram"), Level::Ok);
    }

    #[test]
    fn too_little_page_file_room_blocks() {
        let checks = judge(
            &Machine {
                commit_headroom_mib: COMMIT_NEEDED_MIB - 1,
                ..good()
            },
            &Settings::default(),
        );
        assert_eq!(level(&checks, "page_file"), Level::Blocking);
    }

    #[test]
    fn no_graphics_card_blocks_and_power_saver_warns() {
        let checks = judge(
            &Machine {
                gpu: None,
                power_saver: Some(true),
                ..good()
            },
            &Settings::default(),
        );
        assert_eq!(level(&checks, "gpu"), Level::Blocking);
        assert_eq!(level(&checks, "power_plan"), Level::Warning);
    }

    #[test]
    fn out_of_range_settings_block() {
        let mut s = Settings::default();
        s.performance.ram_budget_mib = Some(1);
        assert_eq!(level(&judge(&good(), &s), "settings"), Level::Blocking);
    }

    #[test]
    fn summary_counts_each_level() {
        let m = Machine {
            gpu: None,
            power_saver: Some(true),
            runtime_present: false,
            ..good()
        };
        assert_eq!(
            summary(&judge(&m, &Settings::default())),
            "7 OK, 2 warnings, 1 blocking"
        );
    }

    #[test]
    fn an_older_driver_than_the_measured_one_warns() {
        let measured = Some(amd([32, 0, 21025, 1010]));
        let with_driver = |version| {
            let m = Machine {
                gpu_driver: Some(amd(version)),
                ..good()
            };
            check(
                &judge_with(&m, &Settings::default(), measured),
                "gpu_driver",
            )
        };
        assert_eq!(with_driver([32, 0, 21025, 1010]).level, Level::Ok);
        assert_eq!(with_driver([32, 0, 21025, 1011]).level, Level::Ok);
        assert_eq!(with_driver([33, 0, 0, 0]).level, Level::Ok);
        let older = with_driver([32, 0, 21025, 1009]);
        assert_eq!(older.level, Level::Warning);
        assert!(
            older
                .detail
                .contains("32.0.21025.1009 is older than 32.0.21025.1010")
        );
        // The parts compare as numbers, left to right: 31.9 is older than 32.0.
        assert_eq!(with_driver([31, 9, 65535, 9999]).level, Level::Warning);
    }

    #[test]
    fn the_driver_is_not_compared_without_a_measurement_or_across_makes() {
        let s = Settings::default();
        let old = Machine {
            gpu_driver: Some(amd([1, 0, 0, 0])),
            ..good()
        };
        let unmeasured = check(&judge_with(&old, &s, None), "gpu_driver");
        assert_eq!(unmeasured.level, Level::Ok);
        assert!(unmeasured.detail.contains("not checked"));

        let other_make = Machine {
            gpu_driver: Some(Driver {
                vendor_id: 0x10DE,
                version: [1, 0, 0, 0],
            }),
            ..good()
        };
        let measured = Some(amd([32, 0, 21025, 1010]));
        let compared = check(&judge_with(&other_make, &s, measured), "gpu_driver");
        assert_eq!(compared.level, Level::Ok);
        assert!(compared.detail.contains("not compared"));

        let unread = Machine {
            gpu_driver: None,
            ..good()
        };
        assert_eq!(
            check(&judge_with(&unread, &s, measured), "gpu_driver").level,
            Level::Ok
        );
        // The driver of the newest entry in docs/measurements.md.
        assert_eq!(MEASURED_DRIVER, Some(amd([32, 0, 32029, 2010])));
    }

    #[test]
    fn a_hard_disk_or_a_nearly_full_drive_warns() {
        let with_drive = |drive| {
            let m = Machine {
                game_drive: drive,
                ..good()
            };
            check(&judge(&m, &Settings::default()), "game_drive")
        };
        let drive = |free_mib, seek_penalty| {
            Some(Drive {
                free_mib,
                seek_penalty,
            })
        };

        assert_eq!(
            with_drive(drive(GAME_DRIVE_FREE_MIB, Some(false))).level,
            Level::Ok
        );
        let low = with_drive(drive(GAME_DRIVE_FREE_MIB - 1, Some(false)));
        assert_eq!(low.level, Level::Warning);
        assert!(low.detail.contains("Less than 2.0 GiB"));

        let hard_disk = with_drive(drive(500 * 1024, Some(true)));
        assert_eq!(hard_disk.level, Level::Warning);
        assert!(hard_disk.detail.contains("hard disk"));

        let both = with_drive(drive(100, Some(true)));
        assert!(both.detail.contains("hard disk") && both.detail.contains("Less than"));

        // An unknown drive type is not a warning, and no drive is not one either.
        assert_eq!(with_drive(drive(500 * 1024, None)).level, Level::Ok);
        assert_eq!(with_drive(None).level, Level::Ok);
    }

    #[test]
    fn an_active_real_time_scanner_is_noted_with_the_cache_folder_and_is_no_warning() {
        let with_scanners = |scanners: Option<Vec<&str>>| {
            let m = Machine {
                scanners: scanners.map(|s| s.into_iter().map(String::from).collect()),
                ..good()
            };
            check(&judge(&m, &Settings::default()), "antivirus")
        };
        assert_eq!(with_scanners(Some(vec![])).level, Level::Ok);
        assert_eq!(with_scanners(None).level, Level::Ok);

        let one = with_scanners(Some(vec!["Windows Defender"]));
        assert_eq!(one.level, Level::Ok);
        assert!(one.detail.starts_with("Windows Defender scans files"));
        assert!(
            one.detail
                .contains(r"C:\Users\someone\AppData\Local\Riptide")
        );

        let two = with_scanners(Some(vec!["Windows Defender", "Other AV"]));
        assert!(
            two.detail
                .starts_with("Windows Defender and Other AV scans")
        );
    }

    #[test]
    fn warnings_from_the_new_checks_do_not_block() {
        let m = Machine {
            game_drive: Some(Drive {
                free_mib: 1,
                seek_penalty: Some(true),
            }),
            scanners: Some(vec!["Windows Defender".into()]),
            gpu_driver: Some(amd([1, 0, 0, 0])),
            ..good()
        };
        let checks = judge_with(&m, &Settings::default(), Some(amd([2, 0, 0, 0])));
        assert_eq!(summary(&checks), "8 OK, 2 warnings");
        assert!(can_play(&checks));
    }

    #[test]
    fn a_product_state_says_whether_real_time_scanning_is_on() {
        // States seen from Windows' Security Center.
        assert!(real_time_scanning(266_240)); // 0x041000: third-party scanner, on
        assert!(real_time_scanning(397_568)); // 0x061100: Windows Defender, on
        assert!(!real_time_scanning(393_472)); // 0x060100: Windows Defender, off
        assert!(!real_time_scanning(262_144)); // 0x040000: third-party scanner, off
    }

    #[test]
    fn gather_reads_this_machine() {
        let m = gather(
            &Settings::default(),
            std::path::Path::new("no-such-riptide.exe"),
        );
        assert!(m.free_ram_mib > 0 && m.commit_headroom_mib > 0);
        assert_eq!(m.game_folder_exists, None);
        assert!(!m.runtime_present);
        assert_eq!(m.game_drive, None);
        assert!(m.cache_folder.ends_with("Riptide"));
        println!(
            "driver {:?}, scanners {:?}, cache folder {}",
            m.gpu_driver, m.scanners, m.cache_folder
        );
    }

    #[test]
    fn gather_reads_the_drive_of_a_folder_that_exists() {
        let mut s = Settings::default();
        s.game.folder = Some(std::env::temp_dir());
        let m = gather(&s, std::path::Path::new("no-such-riptide.exe"));
        let drive = m.game_drive.expect("the temporary folder is on a drive");
        assert!(drive.free_mib > 0);
        println!("temporary folder's drive: {drive:?}");
    }
}
