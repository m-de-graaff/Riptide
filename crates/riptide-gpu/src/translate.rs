//! From a title's pipeline to a host pipeline: each program translated, compiled, and put
//! into the title's own description of the pipeline.
//!
//! `riptide-shader` turns a program into a host shader's source; the shader compiler turns
//! that into what the host runs (`compile.rs`); `hostpipe.rs` makes the pipeline. This
//! module is the order those happen in, and what a draw needs to know afterwards: which
//! root parameter each stage's inputs go to, and what the stage's program expects in them.

use riptide_shader::container::xbox::{HardwareStage, RootParameter};
use windows::Win32::Graphics::Direct3D12::ID3D12PipelineState;

use crate::hostpipe::Shaders;
use crate::pipeline::{Pipeline, Stage};

/// A pipeline as the host draws with it.
pub(crate) struct HostPipeline {
    pub(crate) state: ID3D12PipelineState,
    /// Each stage that has inputs: the host root parameter they go to, and the root
    /// parameters its program was compiled against, which say what goes in which word.
    pub(crate) stages: Vec<(u32, Vec<RootParameter>)>,
}

// SAFETY: a host pipeline state is free-threaded, and the rest is plain data.
unsafe impl Send for HostPipeline {}
// SAFETY: as above.
unsafe impl Sync for HostPipeline {}

/// One program translated and compiled.
struct Translated {
    shader: Vec<u8>,
    parameters: Vec<RootParameter>,
}

/// The hardware stage a program of a pipeline stage runs as when nothing follows it but
/// rasterising, and the host root parameter its inputs go to.
fn place(stage: Stage) -> Option<(HardwareStage, u32)> {
    match stage {
        Stage::Vertex => Some((HardwareStage::Vertex, crate::stage::INPUTS)),
        Stage::Pixel => Some((HardwareStage::Pixel, crate::stage::INPUTS + 1)),
        Stage::Compute => Some((HardwareStage::Compute, crate::stage::INPUTS + 2)),
        _ => None,
    }
}

/// The root parameters of the program in `code` that runs as `stage`.
fn parameters(code: &[u8], stage: HardwareStage) -> Result<Vec<RootParameter>, String> {
    let container = riptide_shader::container::Container::parse(code)
        .map_err(|error| format!("the program is not read: {error}"))?;
    let dxil = container
        .xbox_dxil()
        .map_err(|error| format!("the program is not read: {error}"))?
        .ok_or("the program has no machine code")?;
    let machine_code = dxil
        .machine_code()
        .map_err(|error| format!("the program is not read: {error}"))?;
    machine_code
        .iter()
        .flat_map(|code| &code.programs)
        .find(|program| program.stage == stage)
        .map(|program| program.root_parameters.clone())
        .ok_or_else(|| format!("the program has no {} stage", stage.short_name()))
}

/// Translates and compiles the program `code` of pipeline stage `stage`.
fn translate(code: &[u8], stage: Stage) -> Result<(u32, Translated), String> {
    let (hardware, parameter) =
        place(stage).ok_or_else(|| format!("{stage:?} programs are not translated yet"))?;
    let parameters = parameters(code, hardware)?;
    let shader = emit(code, hardware)?;
    Ok((parameter, Translated { shader, parameters }))
}

/// The compiled host shader for the program in `code` that runs as `stage`.
fn emit(code: &[u8], stage: HardwareStage) -> Result<Vec<u8>, String> {
    let _ = code;
    Err(format!(
        "the translator does not write host shaders yet ({} program {:016x})",
        stage.short_name(),
        crate::census::hash(code)
    ))
}

/// Makes the host pipeline for a title's pipeline.
pub(crate) fn host_pipeline(pipeline: &Pipeline) -> Result<HostPipeline, String> {
    let mut shaders = Shaders::default();
    let mut stages = Vec::new();
    let mut compute = None;
    for (stage, code) in pipeline.programs() {
        let (parameter, translated) = translate(code, *stage)?;
        match stage {
            Stage::Vertex => shaders.vertex = Some(translated.shader),
            Stage::Pixel => shaders.pixel = Some(translated.shader),
            _ => compute = Some(translated.shader),
        }
        stages.push((parameter, translated.parameters));
    }
    let (stream, subobjects) = pipeline.stream();
    let state = match compute {
        Some(shader) => crate::hostpipe::compute(&shader)?,
        None => {
            if crate::hostpipe::completes_shapes(stream, subobjects) {
                return Err("the pipeline draws rectangles, which are not completed yet".to_owned());
            }
            crate::hostpipe::drawing(stream, subobjects, pipeline.layout(), &shaders)?
        }
    };
    Ok(HostPipeline { state, stages })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_stage_has_its_own_root_parameter() {
        assert_eq!(place(Stage::Vertex), Some((HardwareStage::Vertex, 0)));
        assert_eq!(place(Stage::Pixel), Some((HardwareStage::Pixel, 1)));
        assert_eq!(place(Stage::Compute), Some((HardwareStage::Compute, 2)));
        assert_eq!(place(Stage::Hull), None);
    }

    #[test]
    fn bytes_that_are_no_program_say_so() {
        let error = parameters(&[0; 16], HardwareStage::Vertex).unwrap_err();
        assert!(error.contains("not read"), "{error}");
    }
}
