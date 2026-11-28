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
    /// Each stage that has inputs.
    pub(crate) stages: Vec<StageInputs>,
}

/// What a draw writes into one stage's block of inputs besides what the draw itself says.
pub(crate) struct StageInputs {
    /// The host root parameter the block is bound to.
    pub(crate) parameter: u32,
    /// The root parameters the stage's program was compiled against, which say what goes
    /// in which word of user data.
    pub(crate) parameters: Vec<RootParameter>,
    /// The places in the shaders' sampler heap of the samplers the program's root
    /// signature fixes, in the order the translated shader numbers them. They go in the
    /// words from [`SAMPLERS`] on.
    pub(crate) samplers: Vec<u32>,
}

/// The words of a stage's inputs after its user data: what a translated shader is told
/// besides (the design's "The host shader's bindings").
pub(crate) const GROUP_COUNTS: usize = 64;
/// The first of the words that hold the places of root-fixed samplers.
pub(crate) const SAMPLERS: usize = 67;
/// How many root-fixed samplers a shader can have.
pub(crate) const MOST_SAMPLERS: usize = 11;
/// The draw's first instance, and its first vertex or what is added to each index.
pub(crate) const FIRST_INSTANCE: usize = 78;
pub(crate) const FIRST_VERTEX: usize = 79;

// The extras do not run into each other, and end where a stage's inputs end.
const _: () = assert!(
    GROUP_COUNTS + 3 <= SAMPLERS
        && SAMPLERS + MOST_SAMPLERS <= FIRST_INSTANCE
        && FIRST_VERTEX + 1 == crate::stage::WORDS
);

// SAFETY: a host pipeline state is free-threaded, and the rest is plain data.
unsafe impl Send for HostPipeline {}
// SAFETY: as above.
unsafe impl Sync for HostPipeline {}

/// One program translated and compiled.
struct Translated {
    shader: Vec<u8>,
    parameters: Vec<RootParameter>,
    /// The samplers its root signature fixes, as the console GPU holds them.
    samplers: Vec<[u32; 4]>,
    /// How many rows a vertex shader passes on, and which of them must not be
    /// interpolated, a bit each.
    rows: u32,
    flat_rows: u32,
}

/// A host shader's source and what the pipeline has to know about it.
struct Source {
    text: String,
    /// The shader model to compile for, as `vs_6_6`.
    profile: &'static str,
    samplers: Vec<[u32; 4]>,
    rows: u32,
    flat_rows: u32,
}

/// What kind of numbers a render target of console format `format` holds, which a pixel
/// shader's output has to be declared as: 0 fractions, 1 unsigned whole numbers, 2 signed.
fn target_kind(format: u32) -> u8 {
    match crate::view::whole_numbers(format) {
        0 => 0,
        1 => 1,
        _ => 2,
    }
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

/// Translates and compiles the program `code` of pipeline stage `stage`, for render
/// targets of `targets` kinds (see [`target_kind`]).
fn translate(code: &[u8], stage: Stage, targets: [u8; 8]) -> Result<(u32, Translated), String> {
    let (hardware, parameter) =
        place(stage).ok_or_else(|| format!("{stage:?} programs are not translated yet"))?;
    let parameters = parameters(code, hardware)?;
    let source = source(code, hardware, targets)?;
    let shader = compiled(
        &[
            code,
            hardware.short_name().as_bytes(),
            &targets,
            TRANSLATION.as_bytes(),
        ],
        &source.text,
        source.profile,
    )
    .map_err(|said| {
        format!(
            "the translation of {} program {:016x} does not compile: {said}",
            hardware.short_name(),
            crate::census::hash(code)
        )
    })?;
    Ok((
        parameter,
        Translated {
            shader,
            parameters,
            samplers: source.samplers,
            rows: source.rows,
            flat_rows: source.flat_rows,
        },
    ))
}

/// The compiled form of the host shader `text`: from the cache if what it was made from
/// (`from`, and the compiler) was compiled before, and otherwise compiled and kept.
fn compiled(from: &[&[u8]], text: &str, profile: &str) -> Result<Vec<u8>, String> {
    let compiler = crate::compile::identity();
    let mut parts = from.to_vec();
    parts.push(compiler.as_bytes());
    let key = crate::cache::key(&parts);
    if let Some(shader) = key.as_deref().and_then(crate::cache::get) {
        return Ok(shader);
    }
    let shader = crate::compile::compile(text, "main", profile, riptide_shader::ARGUMENTS)?;
    if let Some(key) = &key {
        crate::cache::put(key, &shader);
    }
    Ok(shader)
}

/// What marks a compiled shader as made by this way of translating. It is part of a kept
/// shader's name, so it changes whenever a translation of the same program would come out
/// different: the translator's own version says when what it writes has changed.
const TRANSLATION: &str = riptide_shader::VERSION;

/// The host shader for the program in `code` that runs as `stage`.
fn source(code: &[u8], stage: HardwareStage, targets: [u8; 8]) -> Result<Source, String> {
    use riptide_shader::TargetKind;
    let options = riptide_shader::Options {
        targets: targets.map(|kind| match kind {
            0 => TargetKind::Float,
            1 => TargetKind::Unsigned,
            _ => TargetKind::Signed,
        }),
    };
    let shader = riptide_shader::translate(code, stage, &options).map_err(|error| {
        format!(
            "{} program {:016x} is not translated: {error}",
            stage.short_name(),
            crate::census::hash(code)
        )
    })?;
    dump(code, stage, &shader.source);
    Ok(Source {
        text: shader.source,
        profile: shader.profile,
        samplers: shader.samplers,
        rows: shader.rows,
        flat_rows: shader.flat_rows,
    })
}

/// Writes a host shader's source next to the programs `RIPTIDE_SHADER_DUMP` asks for, as
/// `<hash>.<stage>.hlsl`, for reading when a translation draws wrong.
fn dump(code: &[u8], stage: HardwareStage, text: &str) {
    if let Some(folder) = std::env::var_os("RIPTIDE_SHADER_DUMP") {
        let name = format!(
            "{:016x}.{}.hlsl",
            crate::census::hash(code),
            stage.short_name()
        );
        let _ = std::fs::write(std::path::Path::new(&folder).join(name), text);
    }
}

/// The geometry shader that completes rectangles behind a vertex shader that passes on
/// `rows` rows, of which those in `flat` must not be interpolated.
fn rectangles(rows: u32, flat: u32) -> Result<Vec<u8>, String> {
    let text = riptide_shader::rectangle_geometry_shader(rows, flat);
    compiled(
        &[
            b"rectangles",
            &rows.to_le_bytes(),
            &flat.to_le_bytes(),
            TRANSLATION.as_bytes(),
        ],
        &text,
        "gs_6_6",
    )
    .map_err(|said| format!("the shader that completes rectangles does not compile: {said}"))
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
    let (stream, subobjects) = pipeline.stream();
    // The kinds of the render targets, from their formats in the stream: eight formats,
    // four bytes into the subobject of type 15.
    let mut targets = [0u8; 8];
    if let Some(formats) = pipeline.subobject(15) {
        for (index, kind) in targets.iter_mut().enumerate() {
            let at = 4 + 4 * index;
            if let Some(bytes) = formats.get(at..at + 4) {
                *kind = target_kind(u32::from_le_bytes(bytes.try_into().expect("4 bytes")));
            }
        }
    }
    let mut shaders = Shaders::default();
    let mut stages = Vec::new();
    let mut compute = None;
    let mut rows = (0, 0);
    for (stage, code) in pipeline.programs() {
        let (parameter, translated) = translate(code, *stage, targets)?;
        if translated.samplers.len() > MOST_SAMPLERS {
            return Err(format!(
                "a program has {} samplers fixed by its root signature, more than the {MOST_SAMPLERS} \
                 a stage's inputs have room for",
                translated.samplers.len()
            ));
        }
        match stage {
            Stage::Vertex => {
                rows = (translated.rows, translated.flat_rows);
                shaders.vertex = Some(translated.shader);
            }
            Stage::Pixel => shaders.pixel = Some(translated.shader),
            _ => compute = Some(translated.shader),
        }
        stages.push(StageInputs {
            parameter,
            parameters: translated.parameters,
            samplers: translated
                .samplers
                .iter()
                .map(|words| crate::bindless::sampler(&crate::view::from_hardware(words)))
                .collect(),
        });
    }
    let state = match compute {
        Some(shader) => crate::hostpipe::compute(&shader)?,
        None => {
            if crate::hostpipe::completes_shapes(stream, subobjects) {
                shaders.geometry = Some(rectangles(rows.0, rows.1)?);
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
    fn a_target_is_fractions_or_whole_numbers_by_its_format() {
        assert_eq!(
            (target_kind(28), target_kind(42), target_kind(43)),
            (0, 1, 2)
        );
    }

    #[test]
    fn bytes_that_are_no_program_say_so() {
        let error = parameters(&[0; 16], HardwareStage::Vertex).unwrap_err();
        assert!(error.contains("not read"), "{error}");
    }
}
