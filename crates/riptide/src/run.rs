//! `riptide run`: the runtime's entry point, as the launcher starts it.
//!
//! It reads the settings, joins the session log, runs the preflight checks and refuses with
//! the reason if one blocks, then loads the title and runs it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use riptide_config::Settings;
use riptide_hle::Imports;
use riptide_log::{Level as LogLevel, Logger};
use riptide_pe::Image;
use riptide_preflight::{Check, Level, Machine};

/// The settings file could not be read or is not valid.
pub const EXIT_SETTINGS: i32 = 3;
/// A preflight check blocks starting.
pub const EXIT_BLOCKED: i32 = 4;
/// The game folder holds no title image, or more than one candidate.
pub const EXIT_NO_TITLE: i32 = 5;
/// Modules that ship in a title's own package, beside its executable, and need nothing of
/// the console but what Riptide already answers: they are loaded as the title is, and the
/// title's imports from them are bound to their own functions.
///
/// Only modules named here are loaded: the video player and the two HTTP libraries, which
/// are built on Windows' own HTTP and so reach nothing while Riptide keeps the title
/// offline. The others in a package call the console's graphics interfaces, of which they
/// would reach parts that are not built.
const PACKAGE_MODULES: &[&str] = &["Bink2Scarlett.dll", "libHttpClient.GDK.dll", "XCurl.dll"];

/// Loads the package modules found in `folder`, starts each as a library, and binds the
/// title's imports from it to its exports. One that cannot be loaded is said in the log
/// and left out, and the title's imports from it stay stubs.
fn load_package_modules(
    folder: &Path,
    imports: &mut Imports,
    log: &Log,
) -> Vec<riptide_pe::LoadedImage> {
    let mut loaded = Vec::new();
    for &name in PACKAGE_MODULES {
        let Ok(bytes) = std::fs::read(folder.join(name)) else {
            continue;
        };
        let image = match riptide_pe::load(name, &bytes, &*imports) {
            Ok(image) => image,
            Err(error) => {
                log.error(format!(
                    "The package module {name} could not be loaded: {error}."
                ));
                continue;
            }
        };
        // SAFETY: the module is part of the title the user asked to run, and its imports
        // are bound as the title's are.
        if !unsafe { image.attach_as_library() } {
            log.error(format!("The package module {name} refused to start."));
            continue;
        }
        let exports = image.exports();
        for (export, address) in &exports {
            // SAFETY: the address is the module's own function of that name, which is
            // what the title imports.
            unsafe {
                imports.implement(name, riptide_pe::Symbol::Name(export.clone()), *address);
            }
        }
        log.info(format!(
            "Package module {name} loaded at {:#x}: {} functions.",
            image.base(),
            exports.len()
        ));
        loaded.push(image);
    }
    loaded
}

/// The title image could not be loaded.
pub const EXIT_LOAD: i32 = 6;

/// What `run` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub struct RunArgs {
    /// The settings file. `None`: the default one.
    pub config: Option<PathBuf>,
    /// The session file to append to. `None`: start a session of its own.
    pub log: Option<PathBuf>,
}

/// Reads the arguments after `run`: `--config <file>` and `--log <file>`, each optional, in
/// either order.
pub fn parse(args: &[&str]) -> Result<RunArgs, String> {
    let mut parsed = RunArgs {
        config: None,
        log: None,
    };
    let mut rest = args.iter();
    while let Some(&flag) = rest.next() {
        let slot = match flag {
            "--config" => &mut parsed.config,
            "--log" => &mut parsed.log,
            other => return Err(format!("unknown argument {other}")),
        };
        if slot.is_some() {
            return Err(format!("{flag} is given twice"));
        }
        let value = rest.next().ok_or(format!("{flag} needs a file"))?;
        *slot = Some(PathBuf::from(value));
    }
    Ok(parsed)
}

/// The blocking checks, each as `title: detail`. Empty: the title may start.
pub fn blocking(checks: &[Check]) -> Vec<String> {
    checks
        .iter()
        .filter(|check| check.level == Level::Blocking)
        .map(|check| format!("{}: {}", check.title, check.detail))
        .collect()
}

/// Finds the title's image in the game folder: the one `.exe` file directly in it.
///
/// How a title dumped from a console is laid out is not settled yet. Until it is, a folder
/// with exactly one program image is the only case that is not a guess.
pub fn find_title(folder: &Path) -> Result<PathBuf, String> {
    let entries = std::fs::read_dir(folder).map_err(|error| {
        format!(
            "The game folder {} cannot be read: {error}.",
            folder.display()
        )
    })?;
    let mut images: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        })
        .collect();
    images.sort();
    match images.len() {
        0 => Err(format!(
            "The game folder {} holds no .exe file to load.",
            folder.display()
        )),
        1 => Ok(images.remove(0)),
        count => Err(format!(
            "The game folder {} holds {count} .exe files, and which one is the title is not \
             known: {}.",
            folder.display(),
            images
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// One line that says what the session runs with.
fn settings_line(settings: &Settings) -> String {
    let display = &settings.display;
    let performance = &settings.performance;
    let or_auto = |value: Option<String>| value.unwrap_or_else(|| "automatic".to_owned());
    format!(
        "Settings: console {:?}, {:?} at {}, HDR {}, RAM budget {}, video memory budget {}, \
         promotion {}, missing pipelines {:?}, readout {}.",
        settings.game.console,
        display.window_mode,
        display
            .resolution
            .map_or_else(|| "the monitor's resolution".to_owned(), |r| r.to_string()),
        on_off(display.hdr),
        or_auto(performance.ram_budget_mib.map(|mib| format!("{mib} MiB"))),
        or_auto(performance.vram_budget_mib.map(|mib| format!("{mib} MiB"))),
        on_off(performance.promotion),
        performance.missing_pipeline,
        on_off(performance.readout),
    )
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The session log, shared with the places that log from title threads.
#[derive(Clone)]
struct Log(Arc<Mutex<Logger>>);

impl Log {
    fn write(&self, level: LogLevel, message: impl Into<String>) {
        let mut logger = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        logger.log(level, message);
    }

    fn info(&self, message: impl Into<String>) {
        self.write(LogLevel::Info, message);
    }

    fn error(&self, message: impl Into<String>) {
        self.write(LogLevel::Error, message);
    }
}

/// Runs the title the settings name and returns the process's exit code: the title's own,
/// or one of the `EXIT_` codes if it never started.
pub fn run(args: &RunArgs) -> i32 {
    run_with(args, riptide_preflight::gather)
}

/// [`run`], with the machine's facts read by `gather`. Tests pass facts of their own, so the
/// path past the preflight checks can be run on a machine that would not pass them.
fn run_with(args: &RunArgs, gather: impl FnOnce(&Settings, &Path) -> Machine) -> i32 {
    let logger = match &args.log {
        Some(path) => Logger::append(path, "runtime"),
        None => Logger::start(&riptide_log::default_dir(), "runtime"),
    };
    let log = Log(Arc::new(Mutex::new(logger)));
    log.info(format!(
        "Riptide runtime {} started.",
        env!("CARGO_PKG_VERSION")
    ));

    // From here on a crash names its place in the log, not only on standard error.
    let fault_log = log.clone();
    riptide_pe::report_unhandled_faults_to(move |line| fault_log.error(line));

    let config = args
        .config
        .clone()
        .unwrap_or_else(riptide_config::default_path);
    let settings = match riptide_config::load(&config) {
        Ok(settings) => settings,
        Err(error) => {
            log.error(format!("Not started: {error} ({}).", config.display()));
            return EXIT_SETTINGS;
        }
    };
    log.info(format!("Settings file: {}.", config.display()));
    log.info(settings_line(&settings));

    let own_image = std::env::current_exe().unwrap_or_default();
    let machine = gather(&settings, &own_image);
    let checks = riptide_preflight::judge(&machine, &settings);
    for check in &checks {
        let level = match check.level {
            Level::Ok => LogLevel::Info,
            Level::Warning => LogLevel::Warning,
            Level::Blocking => LogLevel::Error,
        };
        log.write(level, format!("{}: {}", check.title, check.detail));
    }
    let reasons = blocking(&checks);
    if !reasons.is_empty() {
        for reason in reasons {
            log.error(format!("Not started: {reason}"));
        }
        return EXIT_BLOCKED;
    }

    // The game folder check has passed, so a folder is chosen and exists.
    let folder = settings.game.folder.clone().unwrap_or_default();
    let image_path = match find_title(&folder) {
        Ok(path) => path,
        Err(reason) => {
            log.error(format!("Not started: {reason}"));
            return EXIT_NO_TITLE;
        }
    };
    let name = image_path.file_name().map_or_else(
        || "title".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );

    let stub_log = log.clone();
    let mut imports = Imports::with_log(Arc::new(move |line: &str| stub_log.error(line)));
    // Kept until the title has ended: the title calls into them.
    let _package_modules = load_package_modules(&folder, &mut imports, &log);
    let loaded = std::fs::read(&image_path)
        .map_err(|error| format!("{name} cannot be read: {error}."))
        .and_then(|bytes| {
            let image = Image::parse(&bytes)
                .map_err(|error| format!("{name} is not a title image: {error}."))?;
            let census = imports.census(&image);
            log.info(format!(
                "{name}: {}.",
                census.to_string().lines().last().unwrap_or_default()
            ));
            riptide_pe::load(&name, &bytes, &imports)
                .map_err(|error| format!("{name} could not be loaded: {error}."))
        });
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(reason) => {
            log.error(format!("Not started: {reason}"));
            return EXIT_LOAD;
        }
    };

    // The title finds its files beside itself, and asks Windows who it is.
    if let Err(error) = std::env::set_current_dir(&folder) {
        log.error(format!(
            "Not started: the game folder cannot be made the working folder: {error}."
        ));
        return EXIT_LOAD;
    }
    riptide_hle::process::set_title(loaded.base(), &image_path, loaded.exports());
    let trace_log = log.clone();
    riptide_hle::set_trace(Arc::new(move |line: &str| trace_log.info(line)));

    log.info(format!("Running {name}, loaded at {:#x}.", loaded.base()));
    // SAFETY: running the title is what the user asked for. Its imports are bound to
    // Riptide's implementations, to host functions listed as the same, or to stubs that stop
    // the title when called.
    let exit_code = match unsafe { loaded.run_on_own_thread() } {
        Ok(exit_code) => exit_code,
        Err(error) => {
            log.error(format!("{name} could not be started: {error}."));
            return EXIT_LOAD;
        }
    };
    log.info(format!("{name} ended with exit code {exit_code}."));
    exit_code as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_the_launcher_passes_parse() {
        assert_eq!(
            parse(&["--config", "a.toml", "--log", "b.log"]),
            Ok(RunArgs {
                config: Some("a.toml".into()),
                log: Some("b.log".into()),
            })
        );
        assert_eq!(
            parse(&["--log", "b.log"]),
            Ok(RunArgs {
                config: None,
                log: Some("b.log".into()),
            })
        );
        assert_eq!(
            parse(&[]),
            Ok(RunArgs {
                config: None,
                log: None,
            })
        );
    }

    #[test]
    fn bad_arguments_are_named() {
        assert_eq!(parse(&["--config"]), Err("--config needs a file".into()));
        assert_eq!(
            parse(&["--log", "a", "--log", "b"]),
            Err("--log is given twice".into())
        );
        assert_eq!(parse(&["--fast"]), Err("unknown argument --fast".into()));
    }

    fn check(level: Level, title: &'static str, detail: &str) -> Check {
        Check {
            id: "test",
            level,
            title,
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn only_blocking_checks_stop_the_title_and_each_gives_its_reason() {
        let checks = [
            check(Level::Ok, "Graphics card", "Found."),
            check(Level::Warning, "Power plan", "Power saver is active."),
            check(
                Level::Blocking,
                "Free RAM",
                "2.0 GiB free, 16.0 GiB needed.",
            ),
            check(Level::Blocking, "Game folder", "Choose the game's folder."),
        ];
        assert_eq!(
            blocking(&checks),
            [
                "Free RAM: 2.0 GiB free, 16.0 GiB needed.",
                "Game folder: Choose the game's folder."
            ]
        );
        assert!(blocking(&checks[..2]).is_empty());
    }

    fn folder(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("riptide-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data.exe")).unwrap();
        for file in files {
            std::fs::write(dir.join(file), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn the_title_is_the_one_program_image_in_the_folder() {
        // A folder named like a program and files of other kinds are not candidates.
        let dir = folder("one", &["Title.EXE", "readme.txt", "data.bin"]);
        assert_eq!(find_title(&dir), Ok(dir.join("Title.EXE")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_folder_with_no_image_or_several_is_refused_with_the_reason() {
        let none = folder("none", &["readme.txt"]);
        assert!(
            find_title(&none)
                .unwrap_err()
                .contains("holds no .exe file")
        );
        std::fs::remove_dir_all(none).unwrap();

        let two = folder("two", &["a.exe", "b.exe"]);
        let reason = find_title(&two).unwrap_err();
        assert!(reason.contains("holds 2 .exe files"), "{reason}");
        assert!(reason.contains("a.exe, b.exe"), "{reason}");
        std::fs::remove_dir_all(two).unwrap();

        let missing = std::env::temp_dir().join("riptide-run-no-such-folder");
        assert!(find_title(&missing).unwrap_err().contains("cannot be read"));
    }

    /// Set in the child process that the test below starts: the settings file and the
    /// session file, separated by `|`.
    const RUN_CHILD: &str = "RIPTIDE_RUN_CHILD";

    /// The body of that child process. On its own it does nothing.
    #[test]
    fn run_child() {
        let Some(paths) = std::env::var_os(RUN_CHILD) else {
            return;
        };
        let paths = paths.into_string().unwrap();
        let (config, log) = paths.split_once('|').unwrap();
        let args = RunArgs {
            config: Some(config.into()),
            log: Some(log.into()),
        };
        // A machine that can carry a title, whatever this one is: everything else is read
        // from this machine as usual.
        let code = run_with(&args, |settings, runtime| Machine {
            free_ram_mib: 64 * 1024,
            commit_headroom_mib: 64 * 1024,
            gpu: Some("a graphics card".to_owned()),
            ..riptide_preflight::gather(settings, runtime)
        });
        std::process::exit(code);
    }

    #[test]
    fn past_the_checks_the_title_is_loaded_run_and_stopped_at_its_first_unbuilt_import() {
        let dir = folder("child", &[]);
        std::fs::remove_dir(dir.join("data.exe")).unwrap();
        let game = dir.join("game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::copy(
            riptide_testkit::stand_in_title(),
            game.join(riptide_testkit::STAND_IN_TITLE),
        )
        .unwrap();
        let config = dir.join("riptide.toml");
        std::fs::write(&config, format!("[game]\nfolder = '{}'\n", game.display())).unwrap();
        let log = dir.join("session.log");

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::tests::run_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(RUN_CHILD, format!("{}|{}", config.display(), log.display()))
            .output()
            .unwrap()
            .status;
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once("runtime  ").map(|(_, message)| message))
            .collect();

        // Riptide implements none of the functions the stand-in title reports through, so
        // its first call stops it, by name, with the stub's exit code.
        assert_eq!(
            status.code(),
            Some(riptide_hle::UNIMPLEMENTED_IMPORT as i32),
            "{text}"
        );
        assert!(
            lines.contains(
                &"stand-in-title.exe: 7 imports: 1 implemented by Riptide, 1 passed to the \
                  host, 5 stubs."
            ),
            "{text}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("Running stand-in-title.exe, loaded at 0x")),
            "{text}"
        );
        let named: Vec<&&str> = lines
            .iter()
            .filter(|line| line.contains("unimplemented import"))
            .collect();
        assert_eq!(
            named,
            [&"unimplemented import standin_console.dll!StandInReport was called"],
            "{text}"
        );
        // No call site is named here: the stand-in title's first report is a tail call from
        // its TLS callback, which leaves no title frame. riptide-hle's tests cover the line.
        assert!(!text.contains("ended with exit code"), "{text}");
        println!("{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_settings_line_names_every_setting() {
        let line = settings_line(&Settings::default());
        for part in [
            "HDR on",
            "RAM budget automatic",
            "video memory budget automatic",
            "promotion on",
            "readout off",
            "the monitor's resolution",
        ] {
            assert!(line.contains(part), "{part} is missing from: {line}");
        }
    }
}
