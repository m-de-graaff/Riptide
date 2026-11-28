//! The runtime's game area: what the running title is.
//!
//! Built so far: the title's id. Starting another title, and restarting after a crash,
//! name themselves when called.

use crate::package::{config, element};
use crate::trace::trace;

/// The interface id a title asks for: `2549f142-6419-4a06-97b5-931aab7c2f34`.
pub(crate) const INTERFACE: [u8; 16] = [
    0x42, 0xf1, 0x49, 0x25, 0x19, 0x64, 0x06, 0x4a, 0x97, 0xb5, 0x93, 0x1a, 0xab, 0x7c, 0x2f, 0x34,
];

pub(crate) const NAME: &str = "game";

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
/// What the console answers a title whose configuration names no id.
const E_GAME_MISSING_GAME_CONFIG: i32 = 0x87e5_001f_u32 as i32;

/// The title id in a title's configuration: eight hexadecimal digits in `TitleId`.
fn title_id(config: &str) -> Option<u32> {
    u32::from_str_radix(element(config, "TitleId")?.trim(), 16).ok()
}

/// `XGameGetXboxTitleId`: `(id out)`.
///
/// # Safety
/// `out` is null or where the id goes.
unsafe extern "system" fn xbox_title_id(_this: usize, out: *mut u32) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    let Some(id) = config().and_then(title_id) else {
        trace!("XGameGetXboxTitleId: the title's configuration names no title id");
        return E_GAME_MISSING_GAME_CONFIG;
    };
    *out = id;
    trace!("XGameGetXboxTitleId = {id:08X}");
    S_OK
}

/// The methods that are built, by position.
pub(crate) fn methods() -> Vec<(usize, usize)> {
    vec![
        // +0x18 `XGameGetXboxTitleId`.
        (3, xbox_title_id as *const () as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_id_is_the_hexadecimal_number_in_the_configuration() {
        let config = "<Game>\n  <TitleId>7952987A</TitleId>\n</Game>";
        assert_eq!(title_id(config), Some(0x7952_987a));
        assert_eq!(title_id("<Game><TitleId> 1f </TitleId></Game>"), Some(0x1f));
        assert_eq!(title_id("<Game><TitleId>seven</TitleId></Game>"), None);
        assert_eq!(title_id("<Game></Game>"), None);
    }

    #[test]
    fn a_missing_place_for_the_id_is_refused() {
        // SAFETY: null is the case under test.
        let result = unsafe { xbox_title_id(0, std::ptr::null_mut()) };
        assert_eq!(result, E_INVALIDARG);
    }
}
