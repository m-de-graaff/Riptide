//! `chad.dll`: the console's audio effects service, which runs convolutions (reverbs) and
//! other block effects on its audio processor.
//!
//! A title connects with memory of its own, starts streams, and submits a block of samples
//! to a stream at a time; a status the title reads says how many blocks are done.
//!
//! **No effect is computed.** A block is answered at once with silence, so a title's audio
//! code runs and its reverbs add nothing. This goes with the rest of audio, which decodes
//! and mixes nothing yet.

use std::ffi::c_void;
use std::sync::Mutex;

use riptide_pe::Symbol;

use crate::imports::Imports;
use crate::trace::trace;

const MODULE: &str = "chad.dll";

const S_OK: i32 = 0;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_OUTOFMEMORY: i32 = 0x8007_000e_u32 as i32;

/// Stream kind 1 is a convolution; the others are plain block effects.
const CONVOLUTION: u32 = 1;
/// Options that only a stereo stream may have, and one a convolution may not.
const STEREO_OPTIONS: u32 = 2 | 8;
const NOT_FOR_CONVOLUTION: u32 = 4;

struct Service {
    most_streams: u32,
    started: Mutex<u32>,
}

/// What a title starts a stream with.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct StreamDescription {
    kind: u32,
    options: u32,
    /// Samples per channel in a block.
    frames: u32,
    channels: u32,
    response_samples: u32,
    response: *const f32,
}

/// What a title reads to learn how far a stream has got.
#[repr(C)]
struct Status {
    submitted: u32,
    completed: u32,
    reserved: u32,
    result: i32,
}

struct Stream {
    service: *const Service,
    frames: u32,
    channels: u32,
    status: Box<Status>,
}

/// One block to process.
#[repr(C)]
struct Command {
    gains: [f32; 2],
    input: *const f32,
    output: *mut f32,
    reserved: u64,
}

/// Whether a stream description is one the console takes.
fn valid(description: &StreamDescription) -> bool {
    // The fields are not aligned, so each is copied out before it is looked at.
    let (kind, options) = (description.kind, description.options);
    let (frames, channels) = (description.frames, description.channels);
    let shape = if kind == CONVOLUTION {
        (1..=2).contains(&channels)
            && matches!(frames, 0x200 | 0x400)
            && options & NOT_FOR_CONVOLUTION == 0
    } else {
        channels == 1 && matches!(frames, 0x200 | 0x400 | 0x800)
    };
    shape && (options & STEREO_OPTIONS == 0 || channels == 2)
}

/// `XDspConnectWithMaximumStreamLimit`: `(memory, its size, longest response in seconds,
/// most streams, most channels, service out)`.
///
/// # Safety
/// `out` is null or where the service goes.
unsafe extern "system" fn connect(
    memory: *mut c_void,
    bytes: u32,
    response_seconds: u32,
    most_streams: u32,
    most_channels: u32,
    out: *mut *mut Service,
) -> i32 {
    if memory.is_null()
        || !(0x1_0000..=0x1f40_0000).contains(&bytes)
        || response_seconds > 0x80
        || most_streams > 0x100
        || most_channels > 2
    {
        return E_INVALIDARG;
    }
    if out.is_null() {
        return E_POINTER;
    }
    trace!(
        "audio effects connected: {bytes:#x} bytes, responses up to {response_seconds} s, \
         {most_streams} streams; every block comes back silent"
    );
    let service = Box::new(Service {
        most_streams,
        started: Mutex::new(0),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(service)) };
    S_OK
}

/// `XDspDisconnect`.
///
/// # Safety
/// `service` is null or one from [`connect`], disconnected once, with no stream left.
unsafe extern "system" fn disconnect(service: *mut Service) -> i32 {
    if service.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `Box::into_raw` in `connect`.
    drop(unsafe { Box::from_raw(service) });
    S_OK
}

/// `XDspActivate`: `(service, stream description, status out, stream out)`.
///
/// # Safety
/// Each pointer is null or valid; `service` is live.
unsafe extern "system" fn activate(
    service: *const Service,
    description: *const StreamDescription,
    status: *mut *const Status,
    stream: *mut *mut Stream,
) -> i32 {
    if status.is_null() || stream.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's pointers; null is refused.
    let (Some(live), Some(description)) = (unsafe { (service.as_ref(), description.as_ref()) })
    else {
        return E_INVALIDARG;
    };
    if !valid(description) {
        return E_INVALIDARG;
    }
    {
        let mut started = live
            .started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *started >= live.most_streams {
            return E_OUTOFMEMORY;
        }
        *started += 1;
    }
    let made = Box::new(Stream {
        service,
        frames: description.frames,
        channels: description.channels,
        status: Box::new(Status {
            submitted: 0,
            completed: 0,
            reserved: 0,
            result: S_OK,
        }),
    });
    // SAFETY: both checked non-null. The status is on the heap and lives as long as the
    // stream does.
    unsafe {
        status.write(&raw const *made.status);
        stream.write(Box::into_raw(made));
    }
    S_OK
}

/// `XDspDeactivate`.
///
/// # Safety
/// `stream` is null or one from [`activate`], stopped once, whose service is live.
unsafe extern "system" fn deactivate(stream: *mut Stream) -> i32 {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `Box::into_raw` in `activate`.
    let stream = unsafe { Box::from_raw(stream) };
    // SAFETY: the stream's service outlives it.
    let service = unsafe { &*stream.service };
    let mut started = service
        .started
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *started = started.saturating_sub(1);
    S_OK
}

/// `XDspSubmitCommand`: `(stream, block, ticket out)`. The block is done at once: its
/// output is silence, and the status says so.
///
/// # Safety
/// `stream` is null or live; `command` is null or a block whose output has room for the
/// stream's block of samples.
unsafe extern "system" fn submit_command(
    stream: *mut Stream,
    command: *const Command,
    ticket: *mut u32,
) -> i32 {
    // SAFETY: the title's pointers; null is refused.
    let (Some(stream), Some(command)) = (unsafe { (stream.as_mut(), command.as_ref()) }) else {
        return E_INVALIDARG;
    };
    if !command.output.is_null() {
        let samples = (stream.frames * stream.channels) as usize;
        // SAFETY: the output has room for one block.
        unsafe { command.output.write_bytes(0, samples) };
    }
    let status = &raw mut *stream.status;
    // SAFETY: the stream's own status, which the title reads from another thread as plain
    // numbers.
    let next = unsafe {
        let next = (&raw const (*status).submitted)
            .read_volatile()
            .wrapping_add(1);
        (&raw mut (*status).submitted).write_volatile(next);
        (&raw mut (*status).result).write_volatile(S_OK);
        (&raw mut (*status).completed).write_volatile(next);
        next
    };
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(ticket) = unsafe { ticket.as_mut() } {
        *ticket = next;
    }
    S_OK
}

pub(crate) fn register(imports: &mut Imports) {
    let functions: [(&str, usize); 5] = [
        (
            "XDspConnectWithMaximumStreamLimit",
            connect as *const () as usize,
        ),
        ("XDspDisconnect", disconnect as *const () as usize),
        ("XDspActivate", activate as *const () as usize),
        ("XDspDeactivate", deactivate as *const () as usize),
        ("XDspSubmitCommand", submit_command as *const () as usize),
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
    fn a_convolution_stream_answers_each_block_with_silence_and_counts_it() {
        let mut memory = vec![0u8; 0x10_0000];
        let mut service = std::ptr::null_mut();
        let response = vec![0f32; 0x800];
        let description = StreamDescription {
            kind: CONVOLUTION,
            options: 2,
            frames: 0x400,
            channels: 2,
            response_samples: 0x800,
            response: response.as_ptr(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        let mut output = vec![1f32; 0x800];
        let input = vec![0.5f32; 0x800];
        let command = Command {
            gains: [1.0, 1.0],
            input: input.as_ptr(),
            output: output.as_mut_ptr(),
            reserved: 0,
        };
        // SAFETY: valid structures; the service and stream are used as made, closed once.
        unsafe {
            assert_eq!(
                connect(
                    memory.as_mut_ptr().cast(),
                    0x10_0000,
                    24,
                    1,
                    2,
                    &mut service
                ),
                S_OK
            );
            assert_eq!(
                activate(service, &description, &mut status, &mut stream),
                S_OK
            );
            // One stream is the most this service was connected for.
            let (mut other_status, mut other) = (std::ptr::null(), std::ptr::null_mut());
            assert_eq!(
                activate(service, &description, &mut other_status, &mut other),
                E_OUTOFMEMORY
            );
            let mut ticket = 0;
            assert_eq!(submit_command(stream, &command, &mut ticket), S_OK);
            assert_eq!(submit_command(stream, &command, &mut ticket), S_OK);
            assert_eq!(ticket, 2);
            assert_eq!(((*status).submitted, (*status).completed), (2, 2));
            assert!(output.iter().all(|&sample| sample == 0.0));
            assert_eq!(deactivate(stream), S_OK);
            assert_eq!(disconnect(service), S_OK);
        }
    }

    #[test]
    fn only_the_stream_shapes_the_console_takes_are_started() {
        let shape = |kind, options, frames, channels| StreamDescription {
            kind,
            options,
            frames,
            channels,
            response_samples: 0,
            response: std::ptr::null(),
        };
        assert!(valid(&shape(CONVOLUTION, 0, 0x400, 1)));
        assert!(valid(&shape(2, 0, 0x800, 1)));
        // A convolution has no blocks of 0x800, and stereo options need two channels.
        assert!(!valid(&shape(CONVOLUTION, 0, 0x800, 2)));
        assert!(!valid(&shape(CONVOLUTION, 2, 0x400, 1)));
        assert!(!valid(&shape(CONVOLUTION, NOT_FOR_CONVOLUTION, 0x400, 2)));
        assert!(!valid(&shape(2, 0, 0x400, 2)));
    }
}
