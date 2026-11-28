//! What a list has been told to draw with, and the draws themselves.
//!
//! A title sets a pipeline, root signatures and their arguments, descriptor heaps and
//! vertex and index buffers, and then draws. None of the setting calls has a host call of
//! its own: the host pipeline's root signature is not the title's (design T7), so the
//! title's arguments are kept here and turned into what a translated shader reads when a
//! draw is made.
//!
//! A draw is made if the pipeline has a host pipeline. The first draw with a pipeline asks
//! for one to be made, on another thread (`translate.rs`). Until it is there, and for a
//! pipeline whose programs cannot be translated, the draw is counted and not made.
//!
//! What a shader program is given is the console's *user data*: a row of 32-bit words,
//! one per scalar register the program starts with. Each program says which root
//! parameter goes in which words (`riptide-shader` reads that from the program), and
//! [`user_data`] fills the row from the arguments kept here.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

use riptide_shader::container::xbox::RootParameter;
use windows::Win32::Graphics::Direct3D12::{D3D12_INDEX_BUFFER_VIEW, D3D12_VERTEX_BUFFER_VIEW};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use crate::descriptor::Heap;
use crate::list::List;
use crate::pipeline::Pipeline;
use crate::trace;

/// How many root parameters a signature can have: 64 words of root arguments at least one
/// word each.
const PARAMETERS: usize = 64;
/// How many vertex buffers can be bound.
const VERTEX_BUFFERS: usize = 32;
/// How many words of user data a stage has.
pub(crate) const USER_WORDS: usize = 64;

/// One root argument, as the title last set it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Argument {
    /// Never set.
    #[default]
    None,
    /// Root constants: the words set so far, from the parameter's first.
    Constants(Vec<u32>),
    /// A descriptor table: the address of its first descriptor.
    Table(u64),
    /// A root view: the title address of a buffer.
    View(u64),
}

/// A root signature and the arguments set for it.
#[derive(Debug, Default)]
pub(crate) struct Roots {
    /// The title's root signature object, or zero. What a program expects of it is in the
    /// program itself, so nothing reads this; it is kept for the session log.
    #[allow(dead_code)]
    pub(crate) signature: usize,
    pub(crate) arguments: Vec<Argument>,
}

impl Roots {
    fn set(&mut self, index: u32, argument: Argument) {
        let index = index as usize;
        if index >= PARAMETERS {
            return;
        }
        if self.arguments.len() <= index {
            self.arguments.resize(index + 1, Argument::None);
        }
        self.arguments[index] = argument;
    }

    /// Sets `words` of the root constants at `index`, from word `offset` of them.
    fn set_constants(&mut self, index: u32, offset: u32, words: &[u32]) {
        let at = index as usize;
        if at >= PARAMETERS || offset as usize + words.len() > USER_WORDS {
            return;
        }
        if self.arguments.len() <= at {
            self.arguments.resize(at + 1, Argument::None);
        }
        if !matches!(self.arguments[at], Argument::Constants(_)) {
            self.arguments[at] = Argument::Constants(Vec::new());
        }
        if let Argument::Constants(kept) = &mut self.arguments[at] {
            let end = offset as usize + words.len();
            if kept.len() < end {
                kept.resize(end, 0);
            }
            kept[offset as usize..end].copy_from_slice(words);
        }
    }
}

/// A vertex or index buffer: a stretch of title memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Buffer {
    pub(crate) address: u64,
    pub(crate) bytes: u32,
    /// A vertex buffer's bytes from one vertex to the next; an index buffer's format.
    pub(crate) stride_or_format: u32,
}

/// Everything a list has been told to draw with since it was last reset.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// The title's pipeline object, or zero.
    pub(crate) pipeline: usize,
    pub(crate) graphics: Roots,
    pub(crate) compute: Roots,
    /// The addresses of the first descriptor of the view heap and of the sampler heap set.
    pub(crate) heaps: [u64; 2],
    pub(crate) index: Buffer,
    pub(crate) vertex: [Buffer; VERTEX_BUFFERS],
    pub(crate) topology: u32,
    /// The host textures bound as render and depth targets, by their addresses.
    pub(crate) targets: Vec<usize>,
    /// What was set that has to be set again on another host list (see `record.rs`).
    pub(crate) carried: crate::record::Carried,
    /// Whether the host list being recorded has been given what every draw with
    /// translated shaders needs: the heaps, the root signature and the page table.
    bound: bool,
}

impl State {
    /// The list goes on in another host list, which has nothing bound.
    pub(crate) fn on_new_host_list(&mut self) {
        self.bound = false;
    }
}

/// A list's drawing state, in the list.
#[derive(Default)]
pub(crate) struct Kept(UnsafeCell<State>);

impl Kept {
    /// The state, to read or change.
    ///
    /// A list is recorded by one thread at a time, which is the console's rule for a title
    /// and the host's own; so no two of these are in use at once.
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn state(&self) -> &mut State {
        // SAFETY: as this function's documentation says.
        unsafe { &mut *self.0.get() }
    }

    /// Forgets everything, as a reset does.
    pub(crate) fn clear(&self) {
        *self.state() = State::default();
    }
}

/// The user data of a program whose root parameters are `parameters`, from the arguments
/// in `roots` and the heaps set.
///
/// | Root type | The words get |
/// | --- | --- |
/// | 0, root constants | the constants, from the word of them the parameter starts at |
/// | 1, a root view | the buffer's title address, low half then high |
/// | 3, a table | the address of the table's first descriptor, which is below 4 GiB |
/// | 5 and 6, the heaps | the address of the view heap's or the sampler heap's first descriptor |
///
/// A parameter that was never set leaves its words zero.
pub(crate) fn user_data(
    parameters: &[RootParameter],
    roots: &Roots,
    heaps: [u64; 2],
) -> [u32; USER_WORDS] {
    let mut words = [0u32; USER_WORDS];
    for parameter in parameters {
        let first = usize::from(parameter.user_data);
        let count = parameter.register_count as usize;
        let Some(out) = words.get_mut(first..first + count) else {
            continue;
        };
        let argument = roots
            .arguments
            .get(usize::from(parameter.slot))
            .unwrap_or(&Argument::None);
        match (parameter.root_type, argument) {
            (0, Argument::Constants(kept)) => {
                let from = parameter.table_offset_words as usize;
                for (index, word) in out.iter_mut().enumerate() {
                    *word = kept.get(from + index).copied().unwrap_or(0);
                }
            }
            (1, Argument::View(address)) => {
                out[0] = *address as u32;
                if let Some(high) = out.get_mut(1) {
                    *high = (*address >> 32) as u32;
                }
            }
            (3, Argument::Table(address)) => out[0] = *address as u32,
            (5, _) => out[0] = heaps[0] as u32,
            (6, _) => out[0] = heaps[1] as u32,
            _ => {}
        }
    }
    words
}

/// `SetPipelineState`.
extern "system" fn set_pipeline(this: &List, pipeline: *const Pipeline) {
    this.drawing().state().pipeline = pipeline as usize;
}

/// `SetDescriptorHeaps`: `(count, heaps)`. At most one heap of views and one of samplers.
///
/// # Safety
/// `heaps` is null or `count` pointers, each null or a live heap.
unsafe extern "system" fn set_heaps(this: &List, count: u32, heaps: *const *const Heap) {
    let state = this.drawing().state();
    for index in 0..count.min(2) as usize {
        // SAFETY: the title's list of heaps and each heap in it.
        let Some(heap) = (unsafe { heaps.as_ref().and_then(|_| (*heaps.add(index)).as_ref()) })
        else {
            continue;
        };
        let (kind, start) = heap.span();
        if let Some(kept) = state.heaps.get_mut(kind as usize) {
            *kept = start;
        }
    }
}

/// `SetComputeRootSignature`. Setting a signature forgets the arguments set for the one
/// before it, as on PC.
extern "system" fn set_compute_signature(this: &List, signature: usize) {
    this.drawing().state().compute = Roots {
        signature,
        arguments: Vec::new(),
    };
}

/// `SetGraphicsRootSignature`.
extern "system" fn set_graphics_signature(this: &List, signature: usize) {
    this.drawing().state().graphics = Roots {
        signature,
        arguments: Vec::new(),
    };
}

/// `SetComputeRootDescriptorTable`: `(parameter, first descriptor)`.
extern "system" fn set_compute_table(this: &List, index: u32, handle: u64) {
    this.drawing()
        .state()
        .compute
        .set(index, Argument::Table(handle));
}

/// `SetGraphicsRootDescriptorTable`.
extern "system" fn set_graphics_table(this: &List, index: u32, handle: u64) {
    this.drawing()
        .state()
        .graphics
        .set(index, Argument::Table(handle));
}

/// `SetComputeRoot32BitConstant`: `(parameter, value, which word)`.
extern "system" fn set_compute_constant(this: &List, index: u32, value: u32, offset: u32) {
    this.drawing()
        .state()
        .compute
        .set_constants(index, offset, &[value]);
}

/// `SetGraphicsRoot32BitConstant`.
extern "system" fn set_graphics_constant(this: &List, index: u32, value: u32, offset: u32) {
    this.drawing()
        .state()
        .graphics
        .set_constants(index, offset, &[value]);
}

/// The title's words for a call that sets several root constants.
///
/// # Safety
/// `words` is null or `count` words.
unsafe fn constants<'a>(words: *const u32, count: u32) -> &'a [u32] {
    if words.is_null() || count as usize > USER_WORDS {
        return &[];
    }
    // SAFETY: as this function's contract says.
    unsafe { std::slice::from_raw_parts(words, count as usize) }
}

/// `SetComputeRoot32BitConstants`: `(parameter, count, words, first word)`.
///
/// # Safety
/// `words` is null or `count` words.
unsafe extern "system" fn set_compute_constants(
    this: &List,
    index: u32,
    count: u32,
    words: *const u32,
    offset: u32,
) {
    // SAFETY: the title's words.
    let words = unsafe { constants(words, count) };
    this.drawing()
        .state()
        .compute
        .set_constants(index, offset, words);
}

/// `SetGraphicsRoot32BitConstants`.
///
/// # Safety
/// `words` is null or `count` words.
unsafe extern "system" fn set_graphics_constants(
    this: &List,
    index: u32,
    count: u32,
    words: *const u32,
    offset: u32,
) {
    // SAFETY: the title's words.
    let words = unsafe { constants(words, count) };
    this.drawing()
        .state()
        .graphics
        .set_constants(index, offset, words);
}

/// `SetComputeRootConstantBufferView`, `SetComputeRootShaderResourceView` and
/// `SetComputeRootUnorderedAccessView`: `(parameter, buffer address)`. Which of the three
/// it is, the root signature and the program already say.
extern "system" fn set_compute_view(this: &List, index: u32, address: u64) {
    this.drawing()
        .state()
        .compute
        .set(index, Argument::View(address));
}

/// The same three for drawing.
extern "system" fn set_graphics_view(this: &List, index: u32, address: u64) {
    this.drawing()
        .state()
        .graphics
        .set(index, Argument::View(address));
}

/// `D3D12_INDEX_BUFFER_VIEW` and `D3D12_VERTEX_BUFFER_VIEW`: an address, a size, and a
/// format or a stride.
#[repr(C)]
#[derive(Clone, Copy)]
struct BufferView {
    address: u64,
    bytes: u32,
    stride_or_format: u32,
}

impl From<BufferView> for Buffer {
    fn from(view: BufferView) -> Buffer {
        Buffer {
            address: view.address,
            bytes: view.bytes,
            stride_or_format: view.stride_or_format,
        }
    }
}

/// `IASetIndexBuffer`: `(view, or null for none)`.
///
/// # Safety
/// `view` is null or valid.
unsafe extern "system" fn set_index_buffer(this: &List, view: *const BufferView) {
    // SAFETY: the title's view.
    this.drawing().state().index = unsafe { view.as_ref() }
        .copied()
        .map(Buffer::from)
        .unwrap_or_default();
}

/// `IASetVertexBuffers`: `(first slot, count, views, or null to clear them)`.
///
/// # Safety
/// `views` is null or `count` views.
unsafe extern "system" fn set_vertex_buffers(
    this: &List,
    first: u32,
    count: u32,
    views: *const BufferView,
) {
    let state = this.drawing().state();
    for index in 0..count as usize {
        let Some(slot) = state.vertex.get_mut(first as usize + index) else {
            break;
        };
        *slot = if views.is_null() {
            Buffer::default()
        } else {
            // SAFETY: the title's views, `count` of them.
            unsafe { views.add(index).read_unaligned() }.into()
        };
    }
}

/// Notes the topology, which the host list has been given too (see `record.rs`).
pub(crate) fn note_topology(list: &List, topology: u32) {
    list.drawing().state().topology = topology;
}

/// Draws and dispatches not made so far.
static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Counts a draw or dispatch that is not made, and says so the first time and now and
/// then after.
fn skipped(why: &str) {
    let skipped = SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if skipped == 1 {
        trace!("the title's first draw or dispatch is not made: {why}");
    } else if skipped.is_power_of_two() && skipped >= 1024 {
        trace!("{skipped} draws and dispatches not made so far; the last because {why}");
    }
}

/// Whether this draw or dispatch is one of the first that `RIPTIDE_DRAW_LOG=<count>` asks
/// to be shown in full: its programs, what the list had set, and each stage's inputs with
/// the memory they lead to. For finding out why a translated program draws wrong; what it
/// shows is the title's own data, which stays in the session log.
fn show_this_draw() -> bool {
    use std::sync::OnceLock;
    static ASKED: OnceLock<u64> = OnceLock::new();
    static SHOWN: AtomicU64 = AtomicU64::new(0);
    let asked = *ASKED.get_or_init(|| {
        std::env::var("RIPTIDE_DRAW_LOG")
            .ok()
            .and_then(|count| count.parse().ok())
            .unwrap_or(0)
    });
    asked != 0 && SHOWN.fetch_add(1, Ordering::Relaxed) < asked
}

/// Says what a stage of a shown draw is given: its inputs, and for each word of user data
/// that is an address of title memory, the 16 words there (a table of descriptors, as a
/// rule).
fn show_inputs(parameter: u32, words: &[u32; crate::stage::WORDS]) {
    trace!("draw shown: the inputs at host parameter {parameter} are {words:x?}");
    for (register, &at) in words[..USER_WORDS].iter().enumerate() {
        if let Some(there) = words_at(u64::from(at), 16).filter(|_| at != 0) {
            trace!("draw shown: user data {register} leads to {at:#x}: {there:x?}");
        }
    }
}

/// `count` words of title memory at `address`, if all of them are the title's.
fn words_at(address: u64, count: usize) -> Option<Vec<u32>> {
    crate::inplace::find(address, 4 * count as u64)?;
    Some(
        (0..count)
            // SAFETY: the whole stretch is title memory, as just found; read unaligned.
            .map(|index| unsafe { ((address as usize + 4 * index) as *const u32).read_unaligned() })
            .collect(),
    )
}

/// Says what a pipeline is the first time something is drawn with it, for the first few
/// pipelines: which programs, and what the list had bound.
fn show_first_use(state: &State, what: &str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashSet<usize>>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let seen = seen.get_or_insert_with(HashSet::new);
    if seen.len() >= 48 || !seen.insert(state.pipeline) {
        return;
    }
    // SAFETY: the pipeline a list was given is live while the list draws with it.
    let Some(pipeline) = (unsafe { (state.pipeline as *const Pipeline).as_ref() }) else {
        trace!("{what} with no pipeline set");
        return;
    };
    let programs: Vec<String> = pipeline
        .programs()
        .iter()
        .map(|(stage, code)| format!("{stage:?} {:016x}", crate::census::hash(code)))
        .collect();
    let buffers = state
        .vertex
        .iter()
        .filter(|buffer| buffer.address != 0)
        .count();
    trace!(
        "{what} with pipeline {:#x}: programs [{}]; topology {}, {buffers} vertex buffers, \
         index format {}; {}",
        state.pipeline,
        programs.join(", "),
        state.topology,
        state.index.stride_or_format,
        pipeline.shown()
    );
}

/// Gets the host list ready to draw or dispatch with the pipeline that is set: the heaps,
/// root signature and page table if this host list has not been given them, the host
/// pipeline, each stage's inputs, and for a draw the vertex and index buffers. `more` is
/// what the draw itself tells each stage, as words of the stage's inputs and their values.
/// An error says why nothing can be drawn.
fn prepare(list: &List, compute: bool, indexed: bool, more: &[(usize, u32)]) -> Result<(), String> {
    let state = list.drawing().state();
    // SAFETY: the pipeline a list was given is live while the list draws with it.
    let pipeline =
        unsafe { (state.pipeline as *const Pipeline).as_ref() }.ok_or("no pipeline is set")?;
    let host = pipeline
        .host()?
        .ok_or("the pipeline's host pipeline is still being made")?;
    let host_list = list.host();
    let shown = show_this_draw();
    if shown {
        let programs: Vec<String> = pipeline
            .programs()
            .iter()
            .map(|(stage, code)| format!("{stage:?} {:016x}", crate::census::hash(code)))
            .collect();
        trace!(
            "draw shown: programs [{}], compute {compute}, indexed {indexed}, the draw says              {more:x?}; topology {}, targets {:x?}, set before: {:?}",
            programs.join(", "),
            state.topology,
            state.targets,
            state.carried
        );
    }
    // SAFETY: plain calls on the host list, with host objects that live for the run or as
    // long as the title's pipeline, and buffers found in title memory.
    unsafe {
        if !state.bound {
            let heaps = crate::bindless::heaps().ok_or("the host gave no heaps for shaders")?;
            let root = crate::stage::root_signature().ok_or("there is no host root signature")?;
            let table = crate::pagetable::buffer()
                .ok_or("there is no page table for the GPU")?
                .GetGPUVirtualAddress();
            host_list.SetDescriptorHeaps(&heaps);
            host_list.SetGraphicsRootSignature(&root);
            host_list.SetComputeRootSignature(&root);
            host_list.SetGraphicsRootShaderResourceView(crate::stage::PAGE_TABLE, table);
            host_list.SetComputeRootShaderResourceView(crate::stage::PAGE_TABLE, table);
            state.bound = true;
        }
        host_list.SetPipelineState(&host.state);
        let roots = if compute {
            &state.compute
        } else {
            &state.graphics
        };
        for stage in &host.stages {
            let mut words = [0u32; crate::stage::WORDS];
            words[..USER_WORDS].copy_from_slice(&user_data(&stage.parameters, roots, state.heaps));
            for (word, &place) in words[crate::translate::SAMPLERS..]
                .iter_mut()
                .zip(&stage.samplers)
            {
                *word = place;
            }
            for &(word, value) in more {
                words[word] = value;
            }
            if shown {
                show_inputs(stage.parameter, &words);
            }
            let block = crate::stage::block(list.allocator_key(), &words)
                .ok_or("the host gave no memory for a stage's inputs")?;
            if compute {
                host_list.SetComputeRootConstantBufferView(stage.parameter, block);
            } else {
                host_list.SetGraphicsRootConstantBufferView(stage.parameter, block);
            }
        }
        if !compute {
            let slots = pipeline
                .layout()
                .iter()
                .map(|element| element.slot as usize + 1)
                .max()
                .unwrap_or(0)
                .min(VERTEX_BUFFERS);
            let mut views = Vec::with_capacity(slots);
            for buffer in &state.vertex[..slots] {
                views.push(if buffer.address == 0 || buffer.bytes == 0 {
                    D3D12_VERTEX_BUFFER_VIEW::default()
                } else {
                    D3D12_VERTEX_BUFFER_VIEW {
                        BufferLocation: in_place(buffer, "a vertex buffer")?,
                        SizeInBytes: buffer.bytes,
                        StrideInBytes: buffer.stride_or_format,
                    }
                });
            }
            if !views.is_empty() {
                host_list.IASetVertexBuffers(0, Some(&views));
            }
            if indexed {
                let view = D3D12_INDEX_BUFFER_VIEW {
                    BufferLocation: in_place(&state.index, "the index buffer")?,
                    SizeInBytes: state.index.bytes,
                    Format: DXGI_FORMAT(state.index.stride_or_format as i32),
                };
                host_list.IASetIndexBuffer(Some(&view));
            }
        }
    }
    // What this list drew to before can be read by this draw, but not what it draws to.
    list.rest_all_but(&state.targets);
    Ok(())
}

/// Where the host GPU finds a vertex or index buffer, which is title memory.
fn in_place(buffer: &Buffer, what: &str) -> Result<u64, String> {
    crate::inplace::find(buffer.address, u64::from(buffer.bytes))
        .map(|place| place.gpu_address)
        .ok_or_else(|| {
            format!(
                "{what} at {:#x}, {} bytes, is not title memory in one stretch of one bank",
                buffer.address, buffer.bytes
            )
        })
}

/// `DrawInstanced`: `(vertices, instances, first vertex, first instance)`.
extern "system" fn draw_instanced(
    this: &List,
    vertices: u32,
    instances: u32,
    first_vertex: u32,
    first_instance: u32,
) {
    show_first_use(this.drawing().state(), "a draw");
    // The console counts a vertex from the first of the draw and an instance from the
    // first of the draw; the host's shaders count both from zero, so they are told.
    let more = [
        (crate::translate::FIRST_VERTEX, first_vertex),
        (crate::translate::FIRST_INSTANCE, first_instance),
    ];
    match prepare(this, false, false, &more) {
        // SAFETY: a plain call on the host list, which has all the draw needs.
        Ok(()) => unsafe {
            this.host()
                .DrawInstanced(vertices, instances, first_vertex, first_instance);
        },
        Err(why) => skipped(&why),
    }
}

/// `DrawIndexedInstanced`: `(indices, instances, first index, what is added to each index,
/// first instance)`.
extern "system" fn draw_indexed(
    this: &List,
    indices: u32,
    instances: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
) {
    show_first_use(this.drawing().state(), "an indexed draw");
    let more = [
        (crate::translate::FIRST_VERTEX, base_vertex as u32),
        (crate::translate::FIRST_INSTANCE, first_instance),
    ];
    match prepare(this, false, true, &more) {
        // SAFETY: a plain call on the host list, which has all the draw needs.
        Ok(()) => unsafe {
            this.host().DrawIndexedInstanced(
                indices,
                instances,
                first_index,
                base_vertex,
                first_instance,
            );
        },
        Err(why) => skipped(&why),
    }
}

/// `Dispatch`: `(groups across, down and deep)`. The program is told the three counts
/// after its user data.
extern "system" fn dispatch(this: &List, x: u32, y: u32, z: u32) {
    show_first_use(this.drawing().state(), "a dispatch");
    let more = [
        (crate::translate::GROUP_COUNTS, x),
        (crate::translate::GROUP_COUNTS + 1, y),
        (crate::translate::GROUP_COUNTS + 2, z),
    ];
    match prepare(this, true, false, &more) {
        // SAFETY: a plain call on the host list, which has all the dispatch needs.
        Ok(()) => unsafe { this.host().Dispatch(x, y, z) },
        Err(why) => skipped(&why),
    }
}

/// Indirect draws, mesh draws and the console's own dispatch: not built yet. The methods
/// bound to this take different arguments, none of which is read.
extern "system" fn unbuilt_draw(this: &List) {
    show_first_use(this.drawing().state(), "a draw of a kind not built yet");
    skipped("indirect draws, mesh draws and the console's own dispatch are not built yet");
}

/// The methods here by their position in the older list layout.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    let mut methods = vec![
        (25, set_pipeline as *const () as usize),
        (32, set_heaps as *const () as usize),
        (33, set_compute_signature as *const () as usize),
        (34, set_graphics_signature as *const () as usize),
        (35, set_compute_table as *const () as usize),
        (36, set_graphics_table as *const () as usize),
        (37, set_compute_constant as *const () as usize),
        (38, set_graphics_constant as *const () as usize),
        (39, set_compute_constants as *const () as usize),
        (40, set_graphics_constants as *const () as usize),
        (47, set_index_buffer as *const () as usize),
        (48, set_vertex_buffers as *const () as usize),
    ];
    methods.extend([41, 43, 45].map(|slot| (slot, set_compute_view as *const () as usize)));
    methods.extend([42, 44, 46].map(|slot| (slot, set_graphics_view as *const () as usize)));
    methods.extend([
        (12, draw_instanced as *const () as usize),
        (13, draw_indexed as *const () as usize),
        (14, dispatch as *const () as usize),
    ]);
    // Indirect, the console's own dispatch, and the two mesh dispatches.
    methods.extend([59, 78, 98, 127].map(|slot| (slot, unbuilt_draw as *const () as usize)));
    methods
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameter(root_type: u32, slot: u8, user_data: u8, registers: u32) -> RootParameter {
        RootParameter {
            root_type,
            table_offset_words: 0,
            register_count: registers,
            base_register: 0,
            user_data,
            slot,
        }
    }

    #[test]
    fn each_kind_of_root_parameter_fills_its_words() {
        let mut roots = Roots::default();
        roots.set_constants(0, 0, &[7, 8, 9]);
        roots.set(1, Argument::View(0x12_3456_7890));
        roots.set(2, Argument::Table(0x0c5b_0040));
        let parameters = [
            parameter(0, 0, 4, 3),
            parameter(1, 1, 0, 2),
            parameter(3, 2, 2, 1),
            parameter(5, 9, 8, 1),
            parameter(6, 9, 9, 1),
        ];
        let words = user_data(&parameters, &roots, [0x0c5b_0000, 0x17eb_0000]);
        assert_eq!(
            words[..10],
            [
                0x3456_7890,
                0x12,
                0x0c5b_0040,
                0,
                7,
                8,
                9,
                0,
                0x0c5b_0000,
                0x17eb_0000
            ]
        );
        assert!(words[10..].iter().all(|&word| word == 0));
    }

    #[test]
    fn constants_are_taken_from_where_the_parameter_starts_in_them() {
        let mut roots = Roots::default();
        roots.set_constants(3, 2, &[30, 40]);
        roots.set_constants(3, 0, &[10]);
        let part = RootParameter {
            table_offset_words: 2,
            ..parameter(0, 3, 1, 2)
        };
        let words = user_data(&[part], &roots, [0, 0]);
        assert_eq!(words[..4], [0, 30, 40, 0]);
        assert_eq!(roots.arguments[3], Argument::Constants(vec![10, 0, 30, 40]));
    }

    #[test]
    fn what_was_never_set_or_does_not_fit_is_left_zero() {
        let roots = Roots::default();
        let parameters = [parameter(3, 5, 0, 1), parameter(0, 0, 63, 4)];
        assert_eq!(user_data(&parameters, &roots, [0, 0]), [0; USER_WORDS]);
        // A setting beyond what a signature can hold is not kept.
        let mut roots = Roots::default();
        roots.set(64, Argument::Table(1));
        roots.set_constants(0, 60, &[0; 8]);
        assert!(roots.arguments.is_empty());
    }

    #[test]
    fn every_method_has_a_position_of_its_own() {
        let mut slots: Vec<usize> = methods().iter().map(|&(slot, _)| slot).collect();
        let count = slots.len();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), count);
    }
}
