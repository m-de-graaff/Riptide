//! The runtime's accessibility area: closed captions, high contrast, and speech.
//!
//! Built so far: a speech synthesizer can be made and closed and given the default voice,
//! there are no installed voices, nothing is spoken, and high contrast is off. A title
//! that asks for speech is told it failed, which is what it is told on a console whose
//! narrator has no voice. Speaking through Windows' own voices is not built.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::trace::trace;

/// The interface id a title asks for: `d722b373-8c4d-4692-9e51-d6ad9b37aa7d`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x73, 0xb3, 0x22, 0xd7, 0x4d, 0x8c, 0x92, 0x46, 0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d,
];

pub(crate) const NAME: &str = "accessibility";

const S_OK: i32 = 0;
const E_FAIL: i32 = 0x8000_4005_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

/// What a synthesizer handle points at. It holds nothing: no speech is made.
struct Synthesizer;

/// `XHighContrastGetMode`: `(mode out)`. Zero is the ordinary look.
///
/// # Safety
/// `mode` is null or where the mode goes.
unsafe extern "system" fn high_contrast_mode(_this: usize, mode: *mut u32) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(mode) = (unsafe { mode.as_mut() }) else {
        return E_INVALIDARG;
    };
    *mode = 0;
    S_OK
}

/// `XSpeechSynthesizerEnumerateInstalledVoices`: `(context, callback)`. There are none, so
/// the callback is not called.
extern "system" fn installed_voices(_this: usize, _context: *mut c_void, _callback: usize) -> i32 {
    S_OK
}

/// `XSpeechSynthesizerCreate`: `(synthesizer out)`.
///
/// # Safety
/// `out` is null or where the handle goes.
unsafe extern "system" fn create(_this: usize, out: *mut *mut c_void) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    *out = Box::into_raw(Box::new(Synthesizer)).cast();
    trace!("speech synthesizer made; nothing will be spoken");
    S_OK
}

/// `XSpeechSynthesizerCloseHandle`.
///
/// # Safety
/// `synthesizer` is null or a handle from [`create`], closed once.
unsafe extern "system" fn close(_this: usize, synthesizer: *mut c_void) {
    if !synthesizer.is_null() {
        // SAFETY: the handle came from `Box::into_raw` in `create`.
        drop(unsafe { Box::from_raw(synthesizer.cast::<Synthesizer>()) });
    }
}

/// `XSpeechSynthesizerSetDefaultVoice`: `(synthesizer)`.
extern "system" fn set_default_voice(_this: usize, _synthesizer: *mut c_void) -> i32 {
    S_OK
}

/// `XSpeechSynthesizerCreateStreamFromText` and `...FromSsml`: `(synthesizer, text, stream
/// out)`. No speech is made: the call fails and the stream is null.
///
/// # Safety
/// `stream` is null or where the stream handle goes.
unsafe extern "system" fn create_stream(
    _this: usize,
    _synthesizer: *mut c_void,
    _text: *const u8,
    stream: *mut *mut c_void,
) -> i32 {
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if !SHOWN.swap(true, Ordering::Relaxed) {
        trace!("the title asks for speech, which is not built: it is told the speech failed");
    }
    // SAFETY: the title's out-pointer; null is tolerated.
    if let Some(stream) = unsafe { stream.as_mut() } {
        *stream = std::ptr::null_mut();
    }
    E_FAIL
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        (5, high_contrast_mode as *const () as usize),
        (8, installed_voices as *const () as usize),
        (9, create as *const () as usize),
        (10, close as *const () as usize),
        (11, set_default_voice as *const () as usize),
        (13, create_stream as *const () as usize),
        (21, create_stream as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_synthesizer_is_made_and_closed_and_speaks_nothing() {
        let mut synthesizer = std::ptr::null_mut();
        let mut stream: *mut c_void = std::ptr::dangling_mut();
        let mut mode = 9;
        // SAFETY: valid out-pointers, and the null ones under test.
        unsafe {
            assert_eq!(create(0, &mut synthesizer), S_OK);
            assert!(!synthesizer.is_null());
            assert_eq!(set_default_voice(0, synthesizer), S_OK);
            assert_eq!(
                create_stream(0, synthesizer, c"hello".as_ptr().cast(), &mut stream),
                E_FAIL
            );
            assert!(stream.is_null());
            close(0, synthesizer);
            close(0, std::ptr::null_mut());
            assert_eq!(create(0, std::ptr::null_mut()), E_INVALIDARG);
            assert_eq!(high_contrast_mode(0, &mut mode), S_OK);
        }
        assert_eq!(mode, 0);
        assert_eq!(installed_voices(0, std::ptr::null_mut(), 0), S_OK);
    }
}
