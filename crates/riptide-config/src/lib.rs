//! Riptide's settings: one TOML file that the launcher writes and the runtime reads.
//! There are no environment variables and no command-line flags for settings.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

pub const MIN_RAM_BUDGET_MIB: u32 = 8 * 1024;
pub const MIN_VRAM_BUDGET_MIB: u32 = 4 * 1024;
pub const MIN_RESOLUTION: Resolution = Resolution {
    width: 640,
    height: 360,
};
pub const MAX_RESOLUTION: Resolution = Resolution {
    width: 7680,
    height: 4320,
};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleMode {
    #[default]
    SeriesX,
    SeriesS,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Fullscreen,
    #[default]
    Borderless,
    Windowed,
}

/// What to do with a draw whose pipeline is still compiling.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MissingPipeline {
    /// Leave the draw out for a few frames: no stall, brief pop-in.
    #[default]
    SkipDraws,
    /// Wait for the compiler: no pop-in, a stall.
    Wait,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} x {}", self.width, self.height)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub game: Game,
    pub display: Display,
    pub performance: Performance,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Game {
    /// The folder holding the title. `None`: not chosen yet.
    pub folder: Option<PathBuf>,
    pub console: ConsoleMode,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Display {
    /// The monitor's device name, such as `\\.\DISPLAY1`. `None`: the primary monitor.
    pub monitor: Option<String>,
    /// `None`: the monitor's own resolution.
    pub resolution: Option<Resolution>,
    pub window_mode: WindowMode,
    /// Pass HDR10 output through to an HDR display.
    pub hdr: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            monitor: None,
            resolution: None,
            window_mode: WindowMode::default(),
            hdr: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Performance {
    /// `None`: set automatically from free memory at launch.
    pub ram_budget_mib: Option<u32>,
    /// `None`: set automatically from the driver's video-memory budget at launch.
    pub vram_budget_mib: Option<u32>,
    pub missing_pipeline: MissingPipeline,
    /// Copy unchanging data to video memory. Off: everything is used in place (slower, same
    /// picture).
    pub promotion: bool,
    /// Show where each frame's time went.
    pub readout: bool,
}

impl Default for Performance {
    fn default() -> Self {
        Self {
            ram_budget_mib: None,
            vram_budget_mib: None,
            missing_pipeline: MissingPipeline::default(),
            promotion: true,
            readout: false,
        }
    }
}

impl Settings {
    /// Values outside what Riptide accepts, as sentences for the user. Empty: all fine.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(r) = self.display.resolution {
            let ok = (MIN_RESOLUTION.width..=MAX_RESOLUTION.width).contains(&r.width)
                && (MIN_RESOLUTION.height..=MAX_RESOLUTION.height).contains(&r.height);
            if !ok {
                out.push(format!(
                    "Resolution {r} is outside {MIN_RESOLUTION} to {MAX_RESOLUTION}."
                ));
            }
        }
        if let Some(mib) = self.performance.ram_budget_mib
            && mib < MIN_RAM_BUDGET_MIB
        {
            out.push(format!(
                "RAM budget {mib} MiB is below the minimum of {MIN_RAM_BUDGET_MIB} MiB."
            ));
        }
        if let Some(mib) = self.performance.vram_budget_mib
            && mib < MIN_VRAM_BUDGET_MIB
        {
            out.push(format!(
                "Video memory budget {mib} MiB is below the minimum of {MIN_VRAM_BUDGET_MIB} MiB."
            ));
        }
        out
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read(std::io::Error),
    /// The file is not valid settings; the message names the line and the key.
    Parse(String),
    Write(std::io::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "could not read the settings file: {e}"),
            Self::Parse(e) => write!(f, "the settings file is not valid: {e}"),
            Self::Write(e) => write!(f, "could not write the settings file: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// `%APPDATA%\Riptide\riptide.toml`, or `riptide.toml` in the current folder without `APPDATA`.
pub fn default_path() -> PathBuf {
    match std::env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata).join("Riptide").join("riptide.toml"),
        None => PathBuf::from("riptide.toml"),
    }
}

/// Reads settings. A missing file gives the defaults; keys left out keep their defaults.
pub fn load(path: &Path) -> Result<Settings, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(ConfigError::Read(e)),
    }
}

/// Writes settings beside the old file and swaps them in, so a crash cannot leave half a file.
pub fn save(path: &Path, settings: &Settings) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(settings).expect("settings always serialise");
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(ConfigError::Write)?;
    }
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text).map_err(ConfigError::Write)?;
    std::fs::rename(&temp, path).map_err(ConfigError::Write)
}

/// Moves a settings file that would not load to `<name>.bad` and returns where it went.
pub fn set_aside(path: &Path) -> std::io::Result<PathBuf> {
    let bad = path.with_extension("toml.bad");
    std::fs::rename(path, &bad)?;
    Ok(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("riptide-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("riptide.toml")
    }

    fn write_raw(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn missing_file_gives_defaults() {
        assert_eq!(load(&temp_file("missing")).unwrap(), Settings::default());
    }

    #[test]
    fn defaults_are_borderless_hdr_with_promotion() {
        let s = Settings::default();
        assert_eq!(s.display.window_mode, WindowMode::Borderless);
        assert!(s.display.hdr && s.performance.promotion && !s.performance.readout);
        assert!(s.problems().is_empty());
    }

    #[test]
    fn saved_settings_load_back_the_same() {
        let path = temp_file("roundtrip");
        let mut s = Settings::default();
        s.game.folder = Some(PathBuf::from(r"D:\Games\Title"));
        s.game.console = ConsoleMode::SeriesS;
        s.display.monitor = Some(r"\\.\DISPLAY2".into());
        s.display.resolution = Some(Resolution {
            width: 2560,
            height: 1440,
        });
        s.performance.ram_budget_mib = Some(16_384);
        s.performance.missing_pipeline = MissingPipeline::Wait;
        save(&path, &s).unwrap();
        assert_eq!(load(&path).unwrap(), s);
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "temporary file left behind"
        );
    }

    #[test]
    fn file_uses_snake_case_names() {
        let path = temp_file("names");
        let mut s = Settings::default();
        s.game.console = ConsoleMode::SeriesS;
        save(&path, &s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("console = \"series_s\""), "{text}");
        assert!(text.contains("window_mode = \"borderless\""), "{text}");
    }

    #[test]
    fn keys_left_out_keep_their_defaults() {
        let path = temp_file("partial");
        write_raw(&path, "[display]\nhdr = false\n");
        let s = load(&path).unwrap();
        assert!(!s.display.hdr);
        assert_eq!(s.display.window_mode, WindowMode::Borderless);
        assert!(s.performance.promotion);
    }

    #[test]
    fn unknown_key_is_an_error_that_names_it() {
        let path = temp_file("unknown");
        write_raw(&path, "[display]\nhrd = true\n");
        match load(&path) {
            Err(ConfigError::Parse(message)) => assert!(message.contains("hrd"), "{message}"),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn broken_file_can_be_set_aside() {
        let path = temp_file("aside");
        write_raw(&path, "not toml [");
        assert!(load(&path).is_err());
        let bad = set_aside(&path).unwrap();
        assert!(bad.exists() && !path.exists());
        assert_eq!(load(&path).unwrap(), Settings::default());
    }

    #[test]
    fn out_of_range_values_are_reported() {
        let mut s = Settings::default();
        s.display.resolution = Some(Resolution {
            width: 100,
            height: 100,
        });
        s.performance.ram_budget_mib = Some(1024);
        s.performance.vram_budget_mib = Some(512);
        assert_eq!(s.problems().len(), 3);
    }
}
