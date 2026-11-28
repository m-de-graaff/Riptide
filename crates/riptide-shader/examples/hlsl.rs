//! Prints the HLSL of every program in the containers named.
//!
//! ```text
//! cargo run -p riptide-shader --example hlsl -- <container file>...
//! ```
//!
//! The output shows what a program does, constants included, so like the
//! `lift` example and unlike the coverage counts it must not leave the
//! machine when the containers are a real title's.

use riptide_shader::container::xbox::HardwareStage;
use riptide_shader::{Options, translate};

fn main() {
    for path in std::env::args().skip(1) {
        println!("// == {path}");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                println!("// cannot read: {error}");
                continue;
            }
        };
        for stage in [
            HardwareStage::Vertex,
            HardwareStage::Pixel,
            HardwareStage::Compute,
        ] {
            match translate(&bytes, stage, &Options::default()) {
                Ok(shader) => {
                    println!(
                        "// {}: {} rows, flat {:#x}, {} fixed samplers",
                        shader.profile,
                        shader.rows,
                        shader.flat_rows,
                        shader.samplers.len()
                    );
                    print!("{}", shader.source);
                }
                Err(riptide_shader::TranslateError::NoProgram(_)) => {}
                Err(error) => println!("// {}: {error}", stage.short_name()),
            }
        }
    }
}
