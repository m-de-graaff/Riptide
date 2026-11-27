//! Command-line front end.
//!
//! So far it has one command, which inspects a title without running it:
//! `riptide census <image>`. `run` arrives with the pieces it needs.

use std::path::Path;
use std::process::ExitCode;

use riptide_hle::Imports;
use riptide_pe::Image;

const USAGE: &str = "\
usage: riptide <command>

  census <image>   list every function the image imports and what Riptide binds it to:
                   riptide (implemented), host (Windows' own function) or stub (named and
                   refused if called). The image is read, never run.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
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
