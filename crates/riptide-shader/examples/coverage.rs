//! Counts how much of a folder of shader containers the translator covers.
//!
//! ```text
//! cargo run --release -p riptide-shader --example coverage -- <folder> [--list-failures] [--compile]
//! ```
//!
//! Reads every file of the folder as a container, lifts every program in
//! it, writes the HLSL of every lifted program, and prints counts only:
//! programs lifted and emitted by stage, and the most frequent reasons a
//! program was not, by instruction and operation name. With `--compile`
//! every emitted shader is also compiled with the DirectX shader compiler
//! (on every processor at once), and the compiler's most frequent
//! complaints are printed with the numbers taken out. Nothing of a
//! program's contents is printed, so the output can be shared.

#[path = "support/dxc.rs"]
mod dxc;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use riptide_shader::container::Container;
use riptide_shader::container::xbox::HardwareStage;
use riptide_shader::lift::lift;
use riptide_shader::{Options, emit_function};

/// Per stage: programs, lifted, lifted with cross-lane operations, emitted,
/// compiled, written as a dispatch loop.
type Counts = [usize; 6];

/// Prints the most frequent of `reasons`.
fn print_reasons(title: &str, reasons: BTreeMap<(&'static str, String), usize>) {
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    println!("{title}");
    for ((stage, reason), count) in reasons.iter().take(40) {
        println!("{count:6}  {stage}  {reason}");
    }
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let Some(folder) = arguments.next() else {
        eprintln!("usage: coverage <folder> [--list-failures] [--compile]");
        std::process::exit(2);
    };
    let flags: Vec<String> = arguments.collect();
    let list = flags.iter().any(|argument| argument == "--list-failures");
    let compile = flags.iter().any(|argument| argument == "--compile");
    let compiler = if compile {
        let found = dxc::find();
        if found.is_none() {
            eprintln!("no DirectX shader compiler (dxc.exe) was found: nothing is compiled");
        }
        found
    } else {
        None
    };
    let mut files = 0usize;
    let mut unreadable = 0usize;
    let mut stages: BTreeMap<&'static str, Counts> = BTreeMap::new();
    let mut lift_reasons: BTreeMap<(&'static str, String), usize> = BTreeMap::new();
    let mut emit_reasons: BTreeMap<(&'static str, String), usize> = BTreeMap::new();
    // What is to be compiled: file name, stage, profile and source.
    let mut emitted: Vec<(String, &'static str, &'static str, String)> = Vec::new();
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
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let container = Container::parse(&bytes);
        let programs = container
            .as_ref()
            .map_err(|error| *error)
            .and_then(|container| container.xbox_dxil())
            .and_then(|dxil| match dxil {
                Some(dxil) => dxil.machine_code(),
                None => Ok(Vec::new()),
            });
        let (Ok(container), Ok(programs)) = (container, programs) else {
            unreadable += 1;
            continue;
        };
        let inputs = container.input_signature().ok().flatten();
        let outputs = container.output_signature().ok().flatten();
        for code in &programs {
            for program in &code.programs {
                let stage = program.stage.short_name();
                let counts = stages.entry(stage).or_default();
                counts[0] += 1;
                let Ok(data) = code.stage_data(program) else {
                    *lift_reasons
                        .entry((stage, "the stage data cannot be read".to_string()))
                        .or_default() += 1;
                    continue;
                };
                let function = match lift(program, &data) {
                    Ok(function) => function,
                    Err(error) => {
                        if list {
                            println!("{name}: {stage}: {error}");
                        }
                        let reason = format!("{}: {}", error.instruction, error.reason);
                        *lift_reasons.entry((stage, reason)).or_default() += 1;
                        continue;
                    }
                };
                counts[1] += 1;
                if function.has_cross_lane() {
                    counts[2] += 1;
                }
                match emit_function(
                    &function,
                    inputs.as_ref(),
                    outputs.as_ref(),
                    &Options::default(),
                ) {
                    Ok(shader) => {
                        counts[3] += 1;
                        if shader.dispatched {
                            counts[5] += 1;
                        }
                        let profile = match program.stage {
                            HardwareStage::Vertex => "vs_6_6",
                            HardwareStage::Pixel => "ps_6_6",
                            _ => "cs_6_6",
                        };
                        if compiler.is_some() {
                            emitted.push((name.clone(), stage, profile, shader.source));
                        }
                    }
                    Err(error) => {
                        if list {
                            println!("{name}: {stage}: {error}");
                        }
                        let reason = format!("{}: {}", error.op, error.reason);
                        *emit_reasons.entry((stage, reason)).or_default() += 1;
                    }
                }
            }
        }
    }

    let mut compile_reasons: BTreeMap<(&'static str, String), usize> = BTreeMap::new();
    if let Some(compiler) = &compiler {
        let scratch = std::env::temp_dir().join(format!("riptide-coverage-{}", std::process::id()));
        if let Err(error) = std::fs::create_dir_all(&scratch) {
            eprintln!("cannot make {}: {error}", scratch.display());
            std::process::exit(1);
        }
        let next = AtomicUsize::new(0);
        // For each shader: whether it compiled, or the complaint's summary.
        let results: Mutex<Vec<(usize, Option<String>)>> = Mutex::new(Vec::new());
        let workers = std::thread::available_parallelism().map_or(4, |count| count.get());
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let (next, results, emitted, scratch) = (&next, &results, &emitted, &scratch);
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some((_, _, profile, source)) = emitted.get(index) else {
                            break;
                        };
                        let file = scratch.join(format!("shader-{worker}"));
                        let outcome = dxc::compile(
                            compiler,
                            source,
                            profile,
                            riptide_shader::ARGUMENTS,
                            &file,
                        )
                        .err()
                        .map(|said| dxc::summary(&said));
                        if let Ok(mut results) = results.lock() {
                            results.push((index, outcome));
                        }
                    }
                });
            }
        });
        let _ = std::fs::remove_dir_all(&scratch);
        for (index, outcome) in results.into_inner().unwrap_or_default() {
            let (name, stage, _, _) = &emitted[index];
            match outcome {
                None => {
                    if let Some(counts) = stages.get_mut(stage) {
                        counts[4] += 1;
                    }
                }
                Some(summary) => {
                    if list {
                        println!("{name}: {stage}: does not compile: {summary}");
                    }
                    *compile_reasons.entry((stage, summary)).or_default() += 1;
                }
            }
        }
    }

    println!("{files} files, {unreadable} not readable as console shader containers");
    let mut total: Counts = [0; 6];
    let percent = |part: usize, whole: usize| 100.0 * part as f64 / whole.max(1) as f64;
    let print = |stage: &str, counts: &Counts| {
        print!(
            "{stage}: {} programs, {} lifted ({:.1}%), {} of them with cross-lane operations, {} emitted ({:.1}%)",
            counts[0],
            counts[1],
            percent(counts[1], counts[0]),
            counts[2],
            counts[3],
            percent(counts[3], counts[0]),
        );
        if compiler.is_some() {
            print!(
                ", {} compiled ({:.1}%)",
                counts[4],
                percent(counts[4], counts[0])
            );
        }
        println!(", {} written as a dispatch loop", counts[5]);
    };
    for (stage, counts) in &stages {
        print(stage, counts);
        for (sum, count) in total.iter_mut().zip(counts) {
            *sum += count;
        }
    }
    print("all", &total);
    print_reasons(
        "why programs were not lifted (first failure of each program):",
        lift_reasons,
    );
    print_reasons(
        "why lifted programs were not emitted (first failure of each program):",
        emit_reasons,
    );
    if compiler.is_some() {
        print_reasons(
            "what the compiler said of emitted shaders it refused (first error of each):",
            compile_reasons,
        );
    }
}
