//! Finding and running the DirectX shader compiler as a program.
//!
//! The library never runs a compiler: it writes HLSL and stops. The
//! coverage example and the crate's tests want to know that the HLSL
//! compiles, so both include this file. The compiler is not this project's
//! to ship, so it is looked for where `riptide-gpu` looks for it: beside
//! the running program, in the folder `RIPTIDE_DXC` names, in the newest
//! Windows SDK, then in a Vulkan SDK. Where none is found, callers skip.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one compilation may take.
pub const LIMIT: Duration = Duration::from_secs(120);

/// The folders inside `folder`, the one whose name sorts last first.
fn newest_first(folder: &Path) -> Vec<PathBuf> {
    let mut inside: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    inside.sort();
    inside.reverse();
    inside
}

/// The compiler program `dxc.exe`, if one is installed.
pub fn find() -> Option<PathBuf> {
    let mut folders = Vec::new();
    if let Some(beside) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_owned))
    {
        folders.push(beside);
    }
    if let Some(named) = std::env::var_os("RIPTIDE_DXC") {
        folders.push(PathBuf::from(named));
    }
    if let Some(programs) = std::env::var_os("ProgramFiles(x86)") {
        let kits = PathBuf::from(programs)
            .join("Windows Kits")
            .join("10")
            .join("bin");
        folders.extend(
            newest_first(&kits)
                .into_iter()
                .map(|version| version.join("x64")),
        );
    }
    if let Some(sdk) = std::env::var_os("VULKAN_SDK") {
        folders.push(PathBuf::from(sdk).join("Bin"));
    }
    folders.extend(
        newest_first(Path::new("C:/VulkanSDK"))
            .into_iter()
            .map(|version| version.join("Bin")),
    );
    folders
        .into_iter()
        .map(|folder| folder.join("dxc.exe"))
        .find(|file| file.is_file())
}

/// Compiles `source` for `profile` with the entry point `main` and the
/// further `arguments` (the emitter's `ARGUMENTS`). `scratch` is a file
/// name this call may use; the source is written beside it. Returns the
/// compiled program, or what the compiler said. A compilation that takes
/// longer than [`LIMIT`] is stopped and is an error.
pub fn compile(
    compiler: &Path,
    source: &str,
    profile: &str,
    arguments: &[&str],
    scratch: &Path,
) -> Result<Vec<u8>, String> {
    let input = scratch.with_extension("hlsl");
    let output = scratch.with_extension("dxil");
    std::fs::write(&input, source).map_err(|error| error.to_string())?;
    let ran = Command::new(compiler)
        .args(["-T", profile, "-E", "main", "-nologo", "-no-warnings"])
        .args(arguments)
        .arg(&input)
        .arg("-Fo")
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())
        .and_then(|mut child| {
            // The compiler's time goes up steeply with the size of a
            // program; one that takes this long is counted, not waited for.
            let started = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        break child.wait_with_output().map_err(|error| error.to_string());
                    }
                    Ok(None) if started.elapsed() > LIMIT => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(format!(
                            "error: the compiler ran for more than {} seconds",
                            LIMIT.as_secs()
                        ));
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) => break Err(error.to_string()),
                }
            }
        });
    let result = match ran {
        Ok(ran) if ran.status.success() => {
            std::fs::read(&output).map_err(|error| error.to_string())
        }
        Ok(ran) => Err(String::from_utf8_lossy(&ran.stderr).into_owned()),
        Err(error) => Err(error),
    };
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    result
}

/// The first error of what the compiler said, with the file, line and
/// column and every number taken out, so that equal errors of different
/// programs count as one and nothing of a program is in it.
pub fn summary(said: &str) -> String {
    let line = said
        .lines()
        .find(|line| line.contains("error"))
        .unwrap_or_else(|| said.lines().next().unwrap_or("no message"));
    let text = line.split("error: ").last().unwrap_or(line);
    let mut out = String::new();
    let mut in_number = false;
    for character in text.chars() {
        if character.is_ascii_digit() {
            if !in_number {
                out.push('N');
            }
            in_number = true;
        } else {
            in_number = false;
            out.push(character);
        }
    }
    out.chars().take(160).collect()
}
