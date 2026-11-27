//! Checks, before a title starts, that the machine can carry it, and says why if it cannot.
//!
//! [`gather`] reads facts from Windows; [`judge`] turns them into checks and is pure.

use riptide_config::Settings;

mod windows;
pub use crate::windows::gather;

/// The RAM budget used when the setting is automatic, in MiB.
pub const AUTO_RAM_BUDGET_MIB: u64 = 16 * 1024;
/// Page-file room needed to reserve the 14 GiB title block, with 2 GiB to spare, in MiB.
pub const COMMIT_NEEDED_MIB: u64 = 16 * 1024;

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
}

fn gib(mib: u64) -> String {
    format!("{:.1} GiB", mib as f64 / 1024.0)
}

pub fn judge(m: &Machine, s: &Settings) -> Vec<Check> {
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
            gpu: Some("AMD Radeon RX 7900 XTX (driver 32.0.21025.1010)".into()),
            power_saver: Some(false),
            game_folder_exists: Some(true),
            runtime_present: true,
        }
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
        assert_eq!(summary(&checks), "7 OK");
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
            "4 OK, 2 warnings, 1 blocking"
        );
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
    }
}
