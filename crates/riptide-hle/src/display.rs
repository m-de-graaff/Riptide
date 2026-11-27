//! The runtime's display area (`XDisplay`): what the screen can show.
//!
//! Built so far: only what the real title has called.

use crate::trace::trace;

/// The interface id a title asks for: `35f07670-706e-4bfb-9476-090798c5ebf3`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x70, 0x76, 0xf0, 0x35, 0x6e, 0x70, 0xfb, 0x4b, 0x94, 0x76, 0x09, 0x07, 0x98, 0xc5, 0xeb, 0xf3,
];

pub(crate) const NAME: &str = "display";

/// `XDisplayHdrModeResult::Disabled`: the screen stays in standard range.
const HDR_DISABLED: u32 = 2;

/// `XDisplayHdrModeInfo`: the brightness range of a screen in high dynamic range, in nits.
#[repr(C)]
struct HdrModeInfo {
    min_tone_map_luminance: f32,
    max_tone_map_luminance: f32,
    max_full_frame_tone_map_luminance: f32,
}

/// `XDisplayTryEnableHdrMode`: asks for high dynamic range output: `(preference, info out)`.
/// Riptide presents in standard range only, so the answer is that it stays off, and the
/// title renders for a standard screen.
///
/// # Safety
/// `info` is null or where the screen's range goes.
unsafe extern "system" fn try_enable_hdr_mode(
    _this: usize,
    preference: u32,
    info: *mut HdrModeInfo,
) -> u32 {
    if !info.is_null() {
        // SAFETY: checked non-null.
        unsafe {
            info.write(HdrModeInfo {
                min_tone_map_luminance: 0.0,
                max_tone_map_luminance: 0.0,
                max_full_frame_tone_map_luminance: 0.0,
            })
        };
    }
    trace!("XDisplayTryEnableHdrMode(preference {preference}): stays off");
    HDR_DISABLED
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        // +0x20 `XDisplayTryEnableHdrMode`.
        (4, try_enable_hdr_mode as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_dynamic_range_stays_off() {
        let mut info = HdrModeInfo {
            min_tone_map_luminance: 1.0,
            max_tone_map_luminance: 1.0,
            max_full_frame_tone_map_luminance: 1.0,
        };
        // SAFETY: a valid out-pointer, and null.
        unsafe {
            assert_eq!(try_enable_hdr_mode(0, 0, &mut info), HDR_DISABLED);
            assert_eq!(
                try_enable_hdr_mode(0, 0, std::ptr::null_mut()),
                HDR_DISABLED
            );
        }
        assert_eq!(info.max_tone_map_luminance, 0.0);
    }
}
