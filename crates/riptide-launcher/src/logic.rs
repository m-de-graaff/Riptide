//! The launcher's decisions, kept apart from the window so they can be tested.

use riptide_config::Resolution;
use riptide_preflight::{Check, can_play};

/// The smallest resolution offered in the list.
const SMALLEST_OFFERED: Resolution = Resolution {
    width: 1280,
    height: 720,
};

/// The resolutions to offer for a monitor: each once, largest first, none under 1280 x 720.
pub fn resolution_choices(modes: &[Resolution]) -> Vec<Resolution> {
    let mut out: Vec<Resolution> = modes
        .iter()
        .copied()
        .filter(|r| r.width >= SMALLEST_OFFERED.width && r.height >= SMALLEST_OFFERED.height)
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out.dedup();
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayOutcome {
    /// A blocking check stands in the way.
    Blocked,
    /// Nothing blocks, but there is no runtime to start yet.
    NotInstalled,
    Start,
}

pub fn play_outcome(checks: &[Check], runtime_present: bool) -> PlayOutcome {
    if !can_play(checks) {
        PlayOutcome::Blocked
    } else if !runtime_present {
        PlayOutcome::NotInstalled
    } else {
        PlayOutcome::Start
    }
}

/// Which lines the Logs tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFilter {
    All,
    /// Warnings and errors.
    Warnings,
    Errors,
}

impl LogFilter {
    pub fn name(self) -> &'static str {
        match self {
            LogFilter::All => "All",
            LogFilter::Warnings => "Warnings",
            LogFilter::Errors => "Errors",
        }
    }

    pub fn shows(self, level: riptide_log::Level) -> bool {
        use riptide_log::Level;
        match self {
            LogFilter::All => true,
            LogFilter::Warnings => level >= Level::Warning,
            LogFilter::Errors => level == Level::Error,
        }
    }
}

/// What the session list shows for a log file: `riptide-20261127-140322.log` becomes
/// `2026-11-27 14:03:22 UTC`. Any other name is shown as it is.
pub fn session_label(file_name: &str) -> String {
    let stamp = file_name
        .strip_prefix("riptide-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .filter(|stamp| {
            stamp.len() == 15
                && stamp.bytes().enumerate().all(|(i, b)| {
                    if i == 8 {
                        b == b'-'
                    } else {
                        b.is_ascii_digit()
                    }
                })
        });
    match stamp {
        Some(s) => format!(
            "{}-{}-{} {}:{}:{} UTC",
            &s[0..4],
            &s[4..6],
            &s[6..8],
            &s[9..11],
            &s[11..13],
            &s[13..15]
        ),
        None => file_name.to_string(),
    }
}

pub fn mib_to_gib(mib: u32) -> u32 {
    mib / 1024
}

pub fn gib_to_mib(gib: u32) -> u32 {
    gib * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use riptide_preflight::Level;

    fn r(width: u32, height: u32) -> Resolution {
        Resolution { width, height }
    }

    fn check(level: Level) -> Check {
        Check {
            id: "x",
            level,
            title: "X",
            detail: String::new(),
        }
    }

    #[test]
    fn resolutions_are_unique_largest_first_and_not_tiny() {
        let modes = [
            r(1920, 1080),
            r(800, 600),
            r(3840, 2160),
            r(1920, 1080),
            r(1280, 720),
            r(1024, 768),
        ];
        assert_eq!(
            resolution_choices(&modes),
            vec![r(3840, 2160), r(1920, 1080), r(1280, 720)]
        );
    }

    #[test]
    fn a_blocking_check_blocks_play_even_with_a_runtime() {
        assert_eq!(
            play_outcome(&[check(Level::Ok), check(Level::Blocking)], true),
            PlayOutcome::Blocked
        );
    }

    #[test]
    fn without_a_runtime_play_reports_not_installed() {
        assert_eq!(
            play_outcome(&[check(Level::Warning)], false),
            PlayOutcome::NotInstalled
        );
    }

    #[test]
    fn warnings_do_not_stop_play() {
        assert_eq!(
            play_outcome(&[check(Level::Ok), check(Level::Warning)], true),
            PlayOutcome::Start
        );
    }

    #[test]
    fn session_files_are_labelled_by_their_start_time() {
        assert_eq!(
            session_label("riptide-20261127-140322.log"),
            "2026-11-27 14:03:22 UTC"
        );
        assert_eq!(session_label("riptide-old.log"), "riptide-old.log");
        assert_eq!(session_label("notes.txt"), "notes.txt");
    }

    #[test]
    fn each_log_filter_shows_its_levels() {
        use riptide_log::Level as L;
        let shown = |f: LogFilter| [L::Info, L::Warning, L::Error].map(|l| f.shows(l));
        assert_eq!(shown(LogFilter::All), [true, true, true]);
        assert_eq!(shown(LogFilter::Warnings), [false, true, true]);
        assert_eq!(shown(LogFilter::Errors), [false, false, true]);
    }

    #[test]
    fn budgets_convert_between_gib_and_mib() {
        assert_eq!(gib_to_mib(16), 16_384);
        assert_eq!(mib_to_gib(16_384), 16);
    }
}
