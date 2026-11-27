//! Repository checks, run with `cargo xtask <command>`.
//!
//! - `policy` fails if a tracked file looks like a game, console or SDK file.
//! - `policy --staged` checks the files staged for the next commit instead. The pre-commit
//!   hook runs it.
//! - `hooks` points git at the hooks in `.githooks`, once per checkout.

mod policy;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
usage: cargo xtask <command>

  policy            fail if a tracked file looks like a game, console or SDK file
  policy --staged   the same, for the files staged for the next commit
  hooks             make git run the hooks in .githooks (once per checkout)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["policy"] => run_policy(false),
        ["policy", "--staged"] => run_policy(true),
        ["hooks"] => install_hooks(),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::from(2)
        }
    }
}

fn run_policy(staged: bool) -> Result<ExitCode, String> {
    let root = repository_root()?;
    let paths = if staged {
        // Added, copied, modified and renamed files; a deletion brings nothing in.
        git_paths(
            &root,
            &[
                "diff",
                "--cached",
                "--name-only",
                "--diff-filter=ACMR",
                "-z",
            ],
        )?
    } else {
        git_paths(&root, &["ls-files", "-z"])?
    };
    let findings = policy::check_files(&root, &paths)
        .map_err(|error| format!("could not read a file: {error}"))?;
    let which = if staged { "staged" } else { "tracked" };
    if findings.is_empty() {
        println!("policy: {} {which} files, none refused", paths.len());
        return Ok(ExitCode::SUCCESS);
    }
    for finding in &findings {
        eprintln!("refused: {finding}");
    }
    eprintln!(
        "policy: {} of {} {which} files refused. No game, console or SDK files in the \
         repository; tests use inputs built from our own source.",
        findings.len(),
        paths.len()
    );
    Ok(ExitCode::FAILURE)
}

fn install_hooks() -> Result<ExitCode, String> {
    let root = repository_root()?;
    git(&root, &["config", "core.hooksPath", ".githooks"])?;
    println!("hooks: git now runs the hooks in .githooks");
    Ok(ExitCode::SUCCESS)
}

fn repository_root() -> Result<PathBuf, String> {
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let root = git(&here, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(root.trim_end_matches(['\r', '\n'])))
}

/// Runs git for a list of paths separated by NUL bytes, so no path needs unquoting.
fn git_paths(root: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    Ok(git(root, args)?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("git {} gave non-UTF-8", args.join(" ")))
}
