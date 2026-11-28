//! `xapu.dll`: the console's audio decoding service. Opus packets in, sound samples out.
//!
//! A title connects once, gets a block of memory for its packets and its output, and then
//! queues commands per stream (start, decode a packet, reset, stop) and takes one result
//! per command, in order.
//!
//! **No sound is decoded.** A packet's length in time is read from its first bytes, and
//! the result is that much silence. So a title's streams advance at the right pace and
//! its audio code runs, and nothing is heard. Real decoding needs an Opus decoder, which
//! is not here yet.

use std::alloc::Layout;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::trace::trace;

const MODULE: &str = "xapu.dll";

const S_OK: i32 = 0;
const E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_OUTOFMEMORY: i32 = 0x8007_000e_u32 as i32;
/// The service's own results: no result is ready, an unknown command, an unknown or
/// stopped stream, a stream started twice, a packet that is not one.
const PENDING_RESULTS: i32 = 0x8ac8_1001_u32 as i32;
const QUEUE_FULL: i32 = 0x8ac8_1000_u32 as i32;
const INVALID_COMMAND: i32 = 0x8ac8_1002_u32 as i32;
const INVALID_STREAM: i32 = 0x8ac8_1003_u32 as i32;
const ALREADY_ACTIVE: i32 = 0x8ac8_1005_u32 as i32;
const BAD_PACKET: i32 = 0x8ac8_1008_u32 as i32;

/// The one kind of processing built: decoding Opus.
const DECODE_OPUS: u32 = 1;

const ACTIVATE: u32 = 1;
const DEACTIVATE: u32 = 2;
const PROCESS: u32 = 3;
const NOTIFY: u32 = 4;
const RESET: u32 = 5;

/// Command option: set the connection's event when the result is ready.
const SIGNAL: u32 = 1;
/// Connect option: the title wants an event for results.
const RESULT_EVENT: u32 = 8;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        attributes: *const c_void,
        manual_reset: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[repr(C)]
struct ConnectInput {
    process: u32,
    streams: u32,
    commands_per_stream: u32,
    memory: u32,
    frames_per_output: u32,
    options: u32,
}

#[repr(C)]
struct ConnectOutput {
    memory: *mut c_void,
    result_event: *mut c_void,
}

/// What every command starts with, and what its result repeats.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct CommandId {
    kind: u32,
    stream: u32,
    sequence: u32,
    options: u32,
    context: u64,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Activate {
    id: CommandId,
    channels: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Process {
    id: CommandId,
    pitch_ramp: f32,
    target_pitch: f32,
    first_frame: u32,
    frame_count: u32,
    input_length: u32,
    input: *const u8,
    output_length: u32,
    output: *mut u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct ApuResult {
    id: CommandId,
    state: u32,
    state_data: u64,
    microseconds: u64,
    error: i32,
    frames: u32,
    output_length: u32,
    output: *mut u8,
    pitch: f32,
    input_length: u32,
    input: *const u8,
}

struct State {
    /// Each stream's channel count while it is started.
    streams: Vec<Option<u32>>,
    /// Results not yet taken, per stream.
    queued: Vec<u32>,
    results: VecDeque<ApuResult>,
    commands: u32,
    taken: u32,
}

struct Connection {
    memory: *mut u8,
    layout: Layout,
    event: *mut c_void,
    per_stream: u32,
    state: Mutex<State>,
}

fn state(connection: &Connection) -> std::sync::MutexGuard<'_, State> {
    connection
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How many samples at 48 kHz an Opus packet holds, from its first bytes: the frame
/// length its configuration number gives, times its frame count. `None` if the bytes are
/// not the start of a packet.
fn packet_frames(packet: &[u8]) -> Option<usize> {
    let first = *packet.first()?;
    let configuration = first >> 3;
    // Samples per frame at 48 kHz: 2.5 ms is 120.
    let frame = match configuration {
        0..=11 => [480, 960, 1920, 2880][usize::from(configuration % 4)],
        12..=15 => [480, 960][usize::from(configuration % 2)],
        _ => [120, 240, 480, 960][usize::from(configuration % 4)],
    };
    let count = match first & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => usize::from(*packet.get(1)? & 0x3f),
    };
    // A packet holds at most 120 ms.
    (count > 0 && frame * count <= 5760).then_some(frame * count)
}

/// `XApuConnect`: `(what is wanted, memory and event out, connection out)`.
///
/// # Safety
/// Each pointer is null or valid.
unsafe extern "system" fn connect(
    input: *const ConnectInput,
    output: *mut ConnectOutput,
    handle: *mut *mut Connection,
) -> i32 {
    // SAFETY: the title's pointers; null is refused.
    let (Some(input), false, false) = (
        unsafe { input.as_ref() },
        output.is_null(),
        handle.is_null(),
    ) else {
        return E_INVALIDARG;
    };
    if !(1..=500).contains(&input.streams)
        || !(1..=16).contains(&input.commands_per_stream)
        || input.memory == 0
        || input.frames_per_output != 0
    {
        return E_INVALIDARG;
    }
    if input.process != DECODE_OPUS {
        trace!(
            "audio decoder: processing kind {} is not built",
            input.process
        );
        return E_NOTIMPL;
    }
    let Ok(layout) = Layout::from_size_align(input.memory as usize, 16) else {
        return E_INVALIDARG;
    };
    // SAFETY: the layout has a size of at least one byte.
    let memory = unsafe { std::alloc::alloc_zeroed(layout) };
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    let event = if input.options & RESULT_EVENT != 0 {
        // SAFETY: a new unnamed event that resets itself.
        unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) }
    } else {
        std::ptr::null_mut()
    };
    let streams = input.streams as usize;
    trace!(
        "audio decoder connected: {streams} streams, {:#x} bytes of memory, options {:#x}; \
         every packet decodes to silence",
        input.memory, input.options
    );
    let connection = Box::new(Connection {
        memory,
        layout,
        event,
        per_stream: input.commands_per_stream,
        state: Mutex::new(State {
            streams: vec![None; streams],
            queued: vec![0; streams],
            results: VecDeque::new(),
            commands: 0,
            taken: 0,
        }),
    });
    // SAFETY: both checked non-null.
    unsafe {
        output.write(ConnectOutput {
            memory: memory.cast(),
            result_event: event,
        });
        handle.write(Box::into_raw(connection));
    }
    S_OK
}

/// `XApuDisconnect`.
///
/// # Safety
/// `handle` is null or a connection from [`connect`], disconnected once.
unsafe extern "system" fn disconnect(handle: *mut Connection) -> i32 {
    if handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `Box::into_raw` in `connect`.
    let connection = unsafe { Box::from_raw(handle) };
    // SAFETY: the event and the memory are the connection's own.
    unsafe {
        if !connection.event.is_null() {
            CloseHandle(connection.event);
        }
        std::alloc::dealloc(connection.memory, connection.layout);
    }
    S_OK
}

/// Answers a decode command with silence as long as its packet.
///
/// # Safety
/// The command's input and output are null or as long as it says.
unsafe fn process(channels: u32, command: &Process, result: &mut ApuResult) {
    let (input, input_length) = (command.input, command.input_length);
    let (output, output_length) = (command.output, command.output_length);
    result.input = input;
    result.input_length = input_length;
    result.output = output;
    if input.is_null() || output.is_null() || input_length == 0 {
        result.error = E_INVALIDARG;
        return;
    }
    // SAFETY: the title's packet, of that many bytes.
    let packet = unsafe { std::slice::from_raw_parts(input, input_length as usize) };
    let Some(decoded) = packet_frames(packet) else {
        result.error = BAD_PACKET;
        return;
    };
    let channels = channels as usize;
    let first = (command.first_frame as usize).min(decoded);
    let frames = (decoded - first)
        .min(command.frame_count as usize)
        .min(output_length as usize / (4 * channels));
    let bytes = frames * channels * 4;
    // Samples are 32-bit floats, and silence is all zero bits.
    // SAFETY: `bytes` is at most the output's length.
    unsafe { output.write_bytes(0, bytes) };
    result.frames = frames as u32;
    result.output_length = bytes as u32;
}

/// `XApuEnqueueCommand`: `(connection, command, commands so far out)`. The command is done
/// at once and its result queued.
///
/// # Safety
/// `handle` is null or a live connection; `command` is null or a whole command of its kind.
unsafe extern "system" fn enqueue_command(
    handle: *const Connection,
    command: *const CommandId,
    total: *mut u32,
) -> i32 {
    // SAFETY: the title's connection; null is refused.
    let Some(connection) = (unsafe { handle.as_ref() }) else {
        return E_INVALIDARG;
    };
    if command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: every command starts with its id.
    let id = unsafe { command.read_unaligned() };
    let mut state = state(connection);
    let stream = id.stream as usize;
    if stream >= state.streams.len() {
        return INVALID_STREAM;
    }
    if state.queued[stream] >= connection.per_stream {
        return QUEUE_FULL;
    }
    let mut result = ApuResult {
        id,
        state: 0,
        state_data: 0,
        microseconds: 0,
        error: S_OK,
        frames: 0,
        output_length: 0,
        output: std::ptr::null_mut(),
        pitch: 1.0,
        input_length: 0,
        input: std::ptr::null(),
    };
    match id.kind {
        ACTIVATE => {
            // SAFETY: a start command goes on with the channel count.
            let channels = unsafe { command.cast::<Activate>().read_unaligned() }.channels;
            result.error = if state.streams[stream].is_some() {
                ALREADY_ACTIVE
            } else if !(1..=2).contains(&channels) {
                E_INVALIDARG
            } else {
                state.streams[stream] = Some(channels);
                S_OK
            };
        }
        DEACTIVATE => state.streams[stream] = None,
        RESET | NOTIFY => {}
        PROCESS => {
            // SAFETY: a decode command is this structure.
            let command = unsafe { command.cast::<Process>().read_unaligned() };
            match state.streams[stream] {
                // SAFETY: the command's buffers are the title's.
                Some(channels) => unsafe { process(channels, &command, &mut result) },
                None => result.error = INVALID_STREAM,
            }
        }
        _ => return INVALID_COMMAND,
    }
    state.queued[stream] += 1;
    state.commands = state.commands.wrapping_add(1);
    state.results.push_back(result);
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(total) = unsafe { total.as_mut() } {
        *total = state.commands;
    }
    if id.options & SIGNAL != 0 && !connection.event.is_null() {
        // SAFETY: the connection's own event.
        unsafe { SetEvent(connection.event) };
    }
    S_OK
}

/// `XApuDequeueResult`: `(connection, result out)`.
///
/// # Safety
/// `handle` is null or a live connection; `out` is null or room for a result.
unsafe extern "system" fn dequeue_result(handle: *const Connection, out: *mut ApuResult) -> i32 {
    // SAFETY: the title's connection; null is refused.
    let Some(connection) = (unsafe { handle.as_ref() }) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let mut state = state(connection);
    let Some(result) = state.results.pop_front() else {
        return PENDING_RESULTS;
    };
    let stream = result.id.stream as usize;
    state.queued[stream] -= 1;
    state.taken = state.taken.wrapping_add(1);
    // SAFETY: the title passes room for one result.
    unsafe { out.write_unaligned(result) };
    S_OK
}

/// `XApuGetResultCount`: `(connection, results taken so far out)`, returning how many wait.
///
/// # Safety
/// `handle` is null or a live connection; `total` is null or where the count goes.
unsafe extern "system" fn result_count(handle: *const Connection, total: *mut u32) -> u32 {
    // SAFETY: the title's connection; null has no results.
    let Some(connection) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    let state = state(connection);
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(total) = unsafe { total.as_mut() } {
        *total = state.taken;
    }
    state.results.len() as u32
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, usize); 5] = [
        ("XApuConnect", connect as *const () as usize),
        ("XApuDisconnect", disconnect as *const () as usize),
        ("XApuEnqueueCommand", enqueue_command as *const () as usize),
        ("XApuDequeueResult", dequeue_result as *const () as usize),
        ("XApuGetResultCount", result_count as *const () as usize),
    ];
    for (name, address) in functions {
        // SAFETY: each function has the arguments of the console function it is bound as.
        unsafe { imports.implement(MODULE, Symbol::Name(name.to_owned()), address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packets_first_bytes_say_how_long_it_is() {
        // Configuration 31 (20 ms of the wide-band coder), one frame.
        assert_eq!(packet_frames(&[31 << 3]), Some(960));
        // Two frames of the same.
        assert_eq!(packet_frames(&[(31 << 3) | 1]), Some(1920));
        // Configuration 16 (2.5 ms), a count in the second byte.
        assert_eq!(packet_frames(&[(16 << 3) | 3, 4]), Some(480));
        // Configuration 3 (60 ms): three frames would be more than a packet may hold.
        assert_eq!(packet_frames(&[(3 << 3) | 3, 3]), None);
        assert_eq!(packet_frames(&[(16 << 3) | 3]), None);
        assert_eq!(packet_frames(&[]), None);
    }

    #[test]
    fn a_stream_is_started_fed_and_stopped_and_every_command_has_its_result() {
        let input = ConnectInput {
            process: DECODE_OPUS,
            streams: 2,
            commands_per_stream: 2,
            memory: 4096,
            frames_per_output: 0,
            options: RESULT_EVENT,
        };
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        let id = |kind, sequence| CommandId {
            kind,
            stream: 1,
            sequence,
            options: SIGNAL,
            context: 7,
        };
        // SAFETY: valid structures; the connection is used as made and closed once.
        unsafe {
            assert_eq!(connect(&input, &mut output, &mut handle), S_OK);
            assert!(!output.memory.is_null() && !output.result_event.is_null());

            let start = Activate {
                id: id(ACTIVATE, 1),
                channels: 2,
            };
            let mut total = 0;
            assert_eq!(
                enqueue_command(handle, (&raw const start).cast(), &mut total),
                S_OK
            );
            assert_eq!(total, 1);

            let packet = [31u8 << 3, 0, 0];
            let mut samples = [1.0f32; 2 * 960];
            let decode = Process {
                id: id(PROCESS, 2),
                pitch_ramp: 0.0,
                target_pitch: 1.0,
                first_frame: 0,
                frame_count: 960,
                input_length: 3,
                input: packet.as_ptr(),
                output_length: (samples.len() * 4) as u32,
                output: samples.as_mut_ptr().cast(),
            };
            assert_eq!(
                enqueue_command(handle, (&raw const decode).cast(), &mut total),
                S_OK
            );
            // The stream's queue holds two results until one is taken.
            assert_eq!(
                enqueue_command(handle, (&raw const decode).cast(), &mut total),
                QUEUE_FULL
            );

            let mut result = std::mem::zeroed::<ApuResult>();
            assert_eq!(result_count(handle, &mut total), 2);
            assert_eq!(dequeue_result(handle, &mut result), S_OK);
            assert_eq!(({ result.id.sequence }, { result.error }), (1, S_OK));
            assert_eq!(dequeue_result(handle, &mut result), S_OK);
            assert_eq!(({ result.frames }, { result.output_length }), (960, 7680));
            assert!(samples.iter().all(|&sample| sample == 0.0));
            assert_eq!(dequeue_result(handle, &mut result), PENDING_RESULTS);

            let unknown = id(9, 3);
            assert_eq!(
                enqueue_command(handle, &unknown, &mut total),
                INVALID_COMMAND
            );
            assert_eq!(disconnect(handle), S_OK);
            assert_eq!(disconnect(std::ptr::null_mut()), E_INVALIDARG);
        }
    }
}
