//! Prints the intermediate form of every program in the containers named.
//!
//! ```text
//! cargo run -p riptide-shader --example lift -- <container file>...
//! ```
//!
//! The output shows what a program does, constants included, so unlike the
//! coverage counts it must not leave the machine when the containers are a
//! real title's.

use riptide_shader::container::Container;
use riptide_shader::ir::cfg::ControlFlow;
use riptide_shader::lift::lift;

fn main() {
    for path in std::env::args().skip(1) {
        println!("== {path}");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                println!("cannot read: {error}");
                continue;
            }
        };
        let programs = Container::parse(&bytes)
            .and_then(|container| container.xbox_dxil())
            .and_then(|dxil| match dxil {
                Some(dxil) => dxil.machine_code(),
                None => Ok(Vec::new()),
            });
        let programs = match programs {
            Ok(programs) => programs,
            Err(error) => {
                println!("cannot parse: {error}");
                continue;
            }
        };
        for code in &programs {
            for program in &code.programs {
                let lifted = code
                    .stage_data(program)
                    .map_err(|error| error.to_string())
                    .and_then(|data| lift(program, &data).map_err(|error| error.to_string()));
                match lifted {
                    Ok(function) => {
                        let flow = ControlFlow::of(&function);
                        println!(
                            "{} blocks, {} loops, cross-lane: {}",
                            function.blocks.len(),
                            flow.loops.len(),
                            function.has_cross_lane()
                        );
                        print!("{function}");
                    }
                    Err(error) => println!("{}: {error}", program.stage.short_name()),
                }
            }
        }
    }
}
