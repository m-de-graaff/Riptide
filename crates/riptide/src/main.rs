//! Command-line front end.
//!
//! - `riptide run` starts the title the settings name. The launcher's Play runs it.
//! - `riptide census <image>` inspects a title without running it.

mod run;

use std::path::Path;
use std::process::ExitCode;

use riptide_hle::Imports;
use riptide_pe::Image;

const USAGE: &str = "\
usage: riptide <command>

  run [--config <file>] [--log <file>]
                   start the title in the game folder the settings name. Without --config
                   the settings are read from %APPDATA%\\Riptide\\riptide.toml; without --log
                   a new session file is started in %LOCALAPPDATA%\\Riptide\\logs. The
                   launcher's Play passes both.

  census <image>   list every function the image imports and what Riptide binds it to:
                   riptide (implemented), host (Windows' own function) or stub (named and
                   refused if called). The image is read, never run.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["run", rest @ ..] => match run::parse(rest) {
            // The title's exit code is the process's, and it can be any 32-bit value.
            Ok(args) => std::process::exit(run::run(&args)),
            Err(problem) => {
                eprintln!("riptide run: {problem}\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        ["census", image] => census(Path::new(image)),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn census(path: &Path) -> ExitCode {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("riptide: could not read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let image = match Image::parse(&bytes) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("riptide: {} is not a title image: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    println!("{}", Imports::new().census(&image));
    ExitCode::SUCCESS
}
