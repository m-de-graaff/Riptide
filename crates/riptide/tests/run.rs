//! Runs `riptide run` as the launcher does: with a settings file and a session file.

use std::path::{Path, PathBuf};
use std::process::Command;

use riptide_hle::UNIMPLEMENTED_IMPORT;
use riptide_testkit::{STAND_IN_TITLE, stand_in_title};

// The runtime's exit codes, as `src/run.rs` defines them.
const EXIT_USAGE: i32 = 2;
const EXIT_SETTINGS: i32 = 3;
const EXIT_BLOCKED: i32 = 4;
const EXIT_NO_TITLE: i32 = 5;

/// A folder for one test, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("riptide-cli-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Writes a settings file naming `game_folder`, with the smallest RAM budget allowed so
    /// the free-RAM check asks as little of this machine as it can.
    fn settings(&self, game_folder: &Path) -> PathBuf {
        let path = self.path("riptide.toml");
        let text = format!(
            "[game]\nfolder = '{}'\n\n[performance]\nram_budget_mib = 8192\n",
            game_folder.display()
        );
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `riptide run --config <config> --log <log>` and returns its exit code and the
/// session file's text.
fn run(config: &Path, log: &Path) -> (i32, String) {
    let status = Command::new(env!("CARGO_BIN_EXE_riptide"))
        .arg("run")
        .arg("--config")
        .arg(config)
        .arg("--log")
        .arg(log)
        .output()
        .unwrap()
        .status;
    let text = std::fs::read_to_string(log).unwrap_or_default();
    (status.code().expect("the runtime exits with a code"), text)
}

/// The messages of the session's lines written by the runtime.
fn runtime_lines(log: &str) -> Vec<&str> {
    log.lines()
        .filter_map(|line| line.split_once("runtime  ").map(|(_, message)| message))
        .collect()
}

#[test]
fn with_no_game_folder_the_runtime_refuses_and_logs_the_reason() {
    let scratch = Scratch::new("no-folder");
    // A settings file that does not exist gives the defaults: no game folder chosen.
    let (code, log) = run(&scratch.path("missing.toml"), &scratch.path("session.log"));

    assert_eq!(code, EXIT_BLOCKED, "{log}");
    let lines = runtime_lines(&log);
    assert!(lines[0].starts_with("Riptide runtime "), "{log}");
    assert!(
        lines.contains(&"Not started: Game folder: Choose the game's folder on the Game tab."),
        "{log}"
    );
    assert!(!log.contains("Running "), "{log}");
}

#[test]
fn the_runtime_appends_to_the_session_the_launcher_started() {
    let scratch = Scratch::new("append");
    let session = scratch.path("session.log");
    let launcher_line = "2026-11-27T14:03:22.512Z INFO  launcher  Play pressed.\n";
    std::fs::write(&session, launcher_line).unwrap();

    let (_, log) = run(&scratch.path("missing.toml"), &session);

    assert!(log.starts_with(launcher_line), "{log}");
    assert!(log.lines().count() > 3, "{log}");
    // Every line the runtime wrote reads back as a runtime line of the session.
    let read = riptide_log::read(&session);
    assert_eq!(read[0].source, "launcher");
    assert!(
        read[1..].iter().all(|line| line.source == "runtime"),
        "{log}"
    );
}

#[test]
fn a_settings_file_that_is_not_valid_is_refused_with_the_reason() {
    let scratch = Scratch::new("bad-settings");
    let config = scratch.path("riptide.toml");
    std::fs::write(&config, "[game]\nno_such_key = 1\n").unwrap();

    let (code, log) = run(&config, &scratch.path("session.log"));

    assert_eq!(code, EXIT_SETTINGS, "{log}");
    assert!(
        log.contains("Not started: the settings file is not valid"),
        "{log}"
    );
    assert!(log.contains("no_such_key"), "{log}");
}

#[test]
fn the_stand_in_title_starts_and_stops_at_its_first_unbuilt_import() {
    let scratch = Scratch::new("stand-in");
    let game = scratch.path("game");
    std::fs::create_dir_all(&game).unwrap();
    std::fs::copy(stand_in_title(), game.join(STAND_IN_TITLE)).unwrap();

    let (code, log) = run(&scratch.settings(&game), &scratch.path("session.log"));
    let lines = runtime_lines(&log);

    if code == EXIT_BLOCKED {
        // This machine cannot carry a title (too little free RAM or page file, or no
        // graphics card). Then the runtime must say so and start nothing.
        assert!(
            lines.iter().any(|line| line.starts_with("Not started: ")),
            "{log}"
        );
        assert!(!log.contains("Running "), "{log}");
        println!("this machine blocks the title; checked the refusal instead:\n{log}");
        return;
    }

    // The title loads and runs. Riptide implements none of the functions it reports
    // through, so its first call stops it, by name.
    assert_eq!(code, UNIMPLEMENTED_IMPORT as i32, "{log}");
    assert!(
        lines.contains(
            &"stand-in-title.exe: 7 imports: 1 implemented by Riptide, 0 passed to the host, \
              6 stubs."
        ),
        "{log}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Running stand-in-title.exe, loaded at 0x")),
        "{log}"
    );
    let named: Vec<&&str> = lines
        .iter()
        .filter(|line| line.contains("unimplemented import"))
        .collect();
    assert_eq!(
        named,
        [&"unimplemented import standin_console.dll!StandInReport was called"],
        "{log}"
    );
}

#[test]
fn a_game_folder_with_no_image_is_refused() {
    let scratch = Scratch::new("empty-folder");
    let game = scratch.path("game");
    std::fs::create_dir_all(&game).unwrap();
    std::fs::write(game.join("readme.txt"), "nothing to run").unwrap();

    let (code, log) = run(&scratch.settings(&game), &scratch.path("session.log"));

    if code == EXIT_BLOCKED {
        // As above: this machine's own checks stop it first.
        assert!(log.contains("Not started: "), "{log}");
        return;
    }
    assert_eq!(code, EXIT_NO_TITLE, "{log}");
    assert!(log.contains("holds no .exe file to load"), "{log}");
}

#[test]
fn an_unknown_argument_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_riptide"))
        .args(["run", "--fast"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown argument --fast"), "{stderr}");
}
