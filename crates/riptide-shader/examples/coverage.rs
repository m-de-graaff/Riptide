//! Counts how much of a folder of shader containers the lift covers.
//!
//! ```text
//! cargo run --release -p riptide-shader --example coverage -- <folder> [--list-failures]
//! ```
//!
//! Reads every file of the folder as a container, lifts every program in
//! it, and prints counts only: programs lifted by stage, and the most
//! frequent reasons a program was not lifted, by instruction name. Nothing
//! of a program's contents is printed, so the output can be shared.

use std::collections::BTreeMap;

use riptide_shader::container::Container;
use riptide_shader::lift::lift;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let Some(folder) = arguments.next() else {
        eprintln!("usage: coverage <folder> [--list-failures]");
        std::process::exit(2);
    };
    let list = arguments.any(|argument| argument == "--list-failures");
    let mut files = 0usize;
    let mut unreadable = 0usize;
    // Per stage: programs, lifted, lifted with cross-lane operations.
    let mut stages: BTreeMap<&'static str, [usize; 3]> = BTreeMap::new();
    let mut reasons: BTreeMap<(&'static str, String), usize> = BTreeMap::new();
    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("cannot read {folder}: {error}");
            std::process::exit(1);
        }
    };
    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        files += 1;
        let programs = Container::parse(&bytes)
            .and_then(|container| container.xbox_dxil())
            .and_then(|dxil| match dxil {
                Some(dxil) => dxil.machine_code(),
                None => Ok(Vec::new()),
            });
        let Ok(programs) = programs else {
            unreadable += 1;
            continue;
        };
        for code in &programs {
            for program in &code.programs {
                let stage = program.stage.short_name();
                let counts = stages.entry(stage).or_default();
                counts[0] += 1;
                let Ok(data) = code.stage_data(program) else {
                    *reasons
                        .entry((stage, "the stage data cannot be read".to_string()))
                        .or_default() += 1;
                    continue;
                };
                match lift(program, &data) {
                    Ok(function) => {
                        counts[1] += 1;
                        if function.has_cross_lane() {
                            counts[2] += 1;
                        }
                    }
                    Err(error) => {
                        if list {
                            println!(
                                "{}: {stage}: {error}",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            );
                        }
                        let reason = format!("{}: {}", error.instruction, error.reason);
                        *reasons.entry((stage, reason)).or_default() += 1;
                    }
                }
            }
        }
    }
    println!("{files} files, {unreadable} not readable as console shader containers");
    let mut total = [0usize; 3];
    for (stage, counts) in &stages {
        println!(
            "{stage}: {} of {} programs lifted ({:.1}%), {} of them with cross-lane operations",
            counts[1],
            counts[0],
            100.0 * counts[1] as f64 / counts[0].max(1) as f64,
            counts[2]
        );
        for (sum, count) in total.iter_mut().zip(counts) {
            *sum += count;
        }
    }
    println!(
        "all: {} of {} programs lifted ({:.1}%)",
        total[1],
        total[0],
        100.0 * total[1] as f64 / total[0].max(1) as f64
    );
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    println!("why programs were not lifted (first failure of each program):");
    for ((stage, reason), count) in reasons.iter().take(40) {
        println!("{count:6}  {stage}  {reason}");
    }
}
