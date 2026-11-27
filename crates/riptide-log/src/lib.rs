//! Riptide's session log: one text file per launcher session, written by the launcher and
//! by the runtime it starts. See README.md in this folder.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many session files are kept.
pub const KEEP_SESSIONS: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warning => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "INFO" => Some(Level::Info),
            "WARN" => Some(Level::Warning),
            "ERROR" => Some(Level::Error),
            _ => None,
        }
    }
}

/// One line of a session log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// UTC, as `2026-11-27T14:03:22.512Z`. Empty for a line that did not parse.
    pub time: String,
    pub level: Level,
    /// Who wrote it: `launcher` or `runtime`. Empty for a line that did not parse.
    pub source: String,
    pub message: String,
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.time.is_empty() {
            return f.write_str(&self.message);
        }
        // A message stays on one line, so every line of the file is one event.
        let message = self.message.replace('\r', "").replace('\n', "\\n");
        write!(
            f,
            "{} {:<5} {}  {}",
            self.time,
            self.level.tag(),
            self.source,
            message
        )
    }
}

/// Reads one line back. Text that is not in the log's format becomes a plain `Info` line.
pub fn parse_line(text: &str) -> Line {
    let plain = || Line {
        time: String::new(),
        level: Level::Info,
        source: String::new(),
        message: text.to_string(),
    };
    let mut parts = text.splitn(2, ' ');
    let (Some(time), Some(rest)) = (parts.next(), parts.next()) else {
        return plain();
    };
    if time.len() != 24 || !time.ends_with('Z') || time.as_bytes()[10] != b'T' {
        return plain();
    }
    let rest = rest.trim_start();
    let mut parts = rest.splitn(2, ' ');
    let (Some(tag), Some(rest)) = (parts.next(), parts.next()) else {
        return plain();
    };
    let Some(level) = Level::from_tag(tag) else {
        return plain();
    };
    let rest = rest.trim_start();
    let Some((source, message)) = rest.split_once("  ") else {
        return plain();
    };
    Line {
        time: time.to_string(),
        level,
        source: source.to_string(),
        message: message.replace("\\n", "\n"),
    }
}

/// The calendar date and time of day for a moment, in UTC: (year, month, day, hour, minute,
/// second). Howard Hinnant's days-to-civil algorithm.
fn civil(since_epoch: Duration) -> (i64, u32, u32, u32, u32, u32) {
    let secs = since_epoch.as_secs();
    let days = (secs / 86_400) as i64;
    let of_day = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
    )
}

/// `2026-11-27T14:03:22.512Z`
fn timestamp(since_epoch: Duration) -> String {
    let (y, mo, d, h, mi, s) = civil(since_epoch);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        since_epoch.subsec_millis()
    )
}

/// `riptide-20261127-140322.log`
fn session_name(since_epoch: Duration) -> String {
    let (y, mo, d, h, mi, s) = civil(since_epoch);
    format!("riptide-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}.log")
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// `%LOCALAPPDATA%\Riptide\logs`, or `logs` in the current folder without `LOCALAPPDATA`.
pub fn default_dir() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) => PathBuf::from(local).join("Riptide").join("logs"),
        None => PathBuf::from("logs"),
    }
}

/// The session files in `dir`, newest first. An unreadable folder gives none.
pub fn sessions(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("riptide-") && n.ends_with(".log"))
        })
        .collect();
    // The names hold the start time, so sorting by name sorts by time.
    files.sort();
    files.reverse();
    files
}

/// Every line of a session file. A missing or unreadable file gives none.
pub fn read(path: &Path) -> Vec<Line> {
    std::fs::read_to_string(path)
        .map(|text| text.lines().map(parse_line).collect())
        .unwrap_or_default()
}

/// Replaces the user's home folder in `text` with `%USERPROFILE%`, whatever its letter case,
/// so a log can be shared without the Windows user name in every path. A different folder
/// that merely starts with the same letters is left alone.
pub fn redact(text: &str, home: &str) -> String {
    if home.is_empty() {
        return text.to_string();
    }
    let lower_text = text.to_ascii_lowercase();
    let lower_home = home.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(found) = lower_text[from..].find(&lower_home) {
        let at = from + found;
        let end = at + home.len();
        // A home of `C:\Users\Sam` must not match inside `C:\Users\Samuel`.
        let whole = !text[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'));
        out.push_str(&text[from..at]);
        out.push_str(if whole {
            "%USERPROFILE%"
        } else {
            &text[at..end]
        });
        from = end;
    }
    out.push_str(&text[from..]);
    out
}

/// Writes lines to one session file.
pub struct Logger {
    file: Option<File>,
    path: PathBuf,
    source: String,
}

impl Logger {
    /// Starts a new session file in `dir` and deletes all but the newest [`KEEP_SESSIONS`].
    /// If the file cannot be made, the logger still works and writes nothing.
    pub fn start(dir: &Path, source: &str) -> Self {
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join(session_name(now()));
        let logger = Self::append(&path, source);
        for old in sessions(dir).into_iter().skip(KEEP_SESSIONS) {
            let _ = std::fs::remove_file(old);
        }
        logger
    }

    /// Writes to an existing session file, or creates it. The runtime uses this with the path
    /// the launcher gives it.
    pub fn append(path: &Path, source: &str) -> Self {
        Self {
            file: OpenOptions::new().create(true).append(true).open(path).ok(),
            path: path.to_path_buf(),
            source: source.to_string(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one line and flushes it. A failed write is dropped: logging never stops the
    /// program.
    pub fn log(&mut self, level: Level, message: impl Into<String>) {
        let line = Line {
            time: timestamp(now()),
            level,
            source: self.source.clone(),
            message: message.into(),
        };
        if let Some(file) = &mut self.file {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.log(Level::Info, message);
    }

    pub fn warning(&mut self, message: impl Into<String>) {
        self.log(Level::Warning, message);
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.log(Level::Error, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("riptide-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn timestamps_are_utc_calendar_times() {
        assert_eq!(timestamp(Duration::ZERO), "1970-01-01T00:00:00.000Z");
        // 2026-11-27 14:03:22.512 UTC
        let moment = Duration::from_millis(1_795_788_202_512);
        assert_eq!(timestamp(moment), "2026-11-27T14:03:22.512Z");
        assert_eq!(session_name(moment), "riptide-20261127-140322.log");
        // A leap day.
        assert_eq!(
            timestamp(Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn a_line_formats_and_parses_back() {
        let line = Line {
            time: "2026-11-27T14:03:22.512Z".into(),
            level: Level::Warning,
            source: "launcher".into(),
            message: "Free RAM: 7.8 GiB free, 16.0 GiB needed.".into(),
        };
        let text = line.to_string();
        assert_eq!(
            text,
            "2026-11-27T14:03:22.512Z WARN  launcher  Free RAM: 7.8 GiB free, 16.0 GiB needed."
        );
        assert_eq!(parse_line(&text), line);
    }

    #[test]
    fn a_message_with_line_breaks_stays_one_line() {
        let line = Line {
            time: "2026-11-27T14:03:22.512Z".into(),
            level: Level::Error,
            source: "runtime".into(),
            message: "first\nsecond".into(),
        };
        let text = line.to_string();
        assert_eq!(text.lines().count(), 1);
        assert_eq!(parse_line(&text), line);
    }

    #[test]
    fn text_in_another_format_is_kept_as_a_plain_line() {
        for text in [
            "thread 'main' panicked at src/main.rs:4",
            "",
            "2026-11-27 no level here",
        ] {
            let line = parse_line(text);
            assert_eq!(line.message, text);
            assert_eq!(line.level, Level::Info);
            assert!(line.time.is_empty());
            assert_eq!(line.to_string(), text);
        }
    }

    #[test]
    fn a_session_is_written_and_read_back() {
        let dir = temp_dir("session");
        let mut log = Logger::start(&dir, "launcher");
        log.info("started");
        log.warning("careful");
        log.error("broken");
        let lines = read(log.path());
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines.iter().map(|l| l.level).collect::<Vec<_>>(),
            [Level::Info, Level::Warning, Level::Error]
        );
        assert!(
            lines
                .iter()
                .all(|l| l.source == "launcher" && l.time.len() == 24)
        );
        assert_eq!(sessions(&dir), vec![log.path().to_path_buf()]);
    }

    #[test]
    fn a_second_writer_appends_to_the_same_session() {
        let dir = temp_dir("shared");
        let mut launcher = Logger::start(&dir, "launcher");
        launcher.info("starting the runtime");
        let mut runtime = Logger::append(launcher.path(), "runtime");
        runtime.info("hello");
        let sources: Vec<String> = read(launcher.path())
            .into_iter()
            .map(|l| l.source)
            .collect();
        assert_eq!(sources, ["launcher", "runtime"]);
    }

    #[test]
    fn only_the_newest_sessions_are_kept() {
        let dir = temp_dir("prune");
        std::fs::create_dir_all(&dir).unwrap();
        for day in 1..=14 {
            std::fs::write(dir.join(format!("riptide-202001{day:02}-000000.log")), "").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "not a session").unwrap();
        let log = Logger::start(&dir, "launcher");
        let kept = sessions(&dir);
        assert_eq!(kept.len(), KEEP_SESSIONS);
        assert_eq!(kept[0], log.path());
        assert!(!dir.join("riptide-20200101-000000.log").exists());
        assert!(dir.join("riptide-20200114-000000.log").exists());
        assert!(dir.join("notes.txt").exists());
    }

    #[test]
    fn a_logger_that_cannot_write_does_nothing() {
        let dir = temp_dir("blocked");
        std::fs::create_dir_all(&dir).unwrap();
        // A folder where the file should be makes opening it fail.
        let path = dir.join("taken");
        std::fs::create_dir_all(&path).unwrap();
        let mut log = Logger::append(&path, "launcher");
        log.error("nowhere to go");
        assert!(read(&path).is_empty());
    }

    #[test]
    fn the_home_folder_is_redacted_whatever_its_case() {
        let home = r"C:\Users\Sam";
        let text = r"settings at C:\Users\Sam\AppData\x and c:\users\sam\y, not C:\Users\Samuel2";
        assert_eq!(
            redact(text, home),
            r"settings at %USERPROFILE%\AppData\x and %USERPROFILE%\y, not C:\Users\Samuel2"
        );
        assert_eq!(redact(text, ""), text);
    }
}
