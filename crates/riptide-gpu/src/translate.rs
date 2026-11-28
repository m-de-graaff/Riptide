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

/// The compiled host shader for the program in `code` that runs as `stage`: from the
/// cache if this program was compiled before by this translator and compiler, and
/// otherwise translated, compiled and kept.
fn emit(code: &[u8], stage: HardwareStage) -> Result<Vec<u8>, String> {
    let key = crate::cache::key(&[
        code,
        stage.short_name().as_bytes(),
        TRANSLATION.as_bytes(),
        crate::compile::identity().as_bytes(),
    ]);
    if let Some(shader) = key.as_deref().and_then(crate::cache::get) {
        return Ok(shader);
    }
    let (source, profile) = source(code, stage)?;
    let shader = crate::compile::compile(&source, "main", profile, &[]).map_err(|said| {
        format!(
            "the translation of {} program {:016x} does not compile: {said}",
            stage.short_name(),
            crate::census::hash(code)
        )
    })?;
    if let Some(key) = &key {
        crate::cache::put(key, &shader);
    }
    Ok(shader)
}

/// What marks a compiled shader as made by this way of translating. It is part of a kept
/// shader's name, so it changes whenever a translation of the same program would come out
/// different: bump it with any change to what `source` or `riptide-shader` writes.
const TRANSLATION: &str = "0";

/// The host shader's source for the program in `code` that runs as `stage`, and the
/// shader model to compile it for.
fn source(code: &[u8], stage: HardwareStage) -> Result<(String, &'static str), String> {
    Err(format!(
        "the translator does not write host shaders yet ({} program {:016x})",
        stage.short_name(),
        crate::census::hash(code)
    ))
}

/// Pipelines waiting to have their host pipeline made, and the threads that make them.
struct Makers {
    waiting: std::sync::Mutex<std::collections::VecDeque<usize>>,
    arrived: std::sync::Condvar,
}

/// Has the host pipeline of the pipeline at `pipeline` made on one of the threads kept
/// for that. The threads are started the first time: a quarter of the machine's, at
/// least two, so that a burst of new pipelines neither waits on one thread nor takes the
/// machine from the title.
pub(crate) fn make_later(pipeline: usize) {
    use std::sync::OnceLock;
    static MAKERS: OnceLock<&'static Makers> = OnceLock::new();
    let makers = MAKERS.get_or_init(|| {
        let makers: &'static Makers = Box::leak(Box::new(Makers {
            waiting: std::sync::Mutex::new(std::collections::VecDeque::new()),
            arrived: std::sync::Condvar::new(),
        }));
        let threads =
            std::thread::available_parallelism().map_or(2, |count| (count.get() / 4).max(2));
        for _ in 0..threads {
            let spawned = std::thread::Builder::new()
                .name("riptide shaders".to_owned())
                .spawn(move || {
                    loop {
                        let pipeline = {
                            let mut waiting = makers
                                .waiting
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            loop {
                                if let Some(pipeline) = waiting.pop_front() {
                                    break pipeline;
                                }
                                waiting = makers
                                    .arrived
                                    .wait(waiting)
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                            }
                        };
                        // SAFETY: the address `Pipeline::host` gave, held until now.
                        unsafe { Pipeline::make_host(pipeline) };
                    }
                });
            if let Err(error) = spawned {
                crate::trace!("a thread for making pipelines could not be started: {error}");
            }
        }
        makers
    });
    makers
        .waiting
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(pipeline);
    makers.arrived.notify_one();
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
