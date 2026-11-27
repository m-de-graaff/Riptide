//! The monitors attached to this PC and the resolutions each offers, read from Windows.

use riptide_config::Resolution;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICEW,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

#[derive(Clone, Debug)]
pub struct Monitor {
    /// Windows' device name, such as `\\.\DISPLAY1`. This is what the settings file stores.
    pub device: String,
    pub primary: bool,
    pub current: Resolution,
    pub modes: Vec<Resolution>,
}

impl Monitor {
    /// What the list shows, such as "Display 1 (2560 x 1440)".
    pub fn label(&self) -> String {
        let number = self.device.trim_start_matches(r"\\.\DISPLAY");
        format!(
            "Display {number} ({}){}",
            self.current,
            if self.primary { ", primary" } else { "" }
        )
    }
}

fn mode(device: &[u16; 32], index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<Resolution> {
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `device` is a NUL-terminated name from EnumDisplayDevicesW, and `dm` is a valid
    // DEVMODEW with its size set.
    let found = unsafe { EnumDisplaySettingsW(PCWSTR(device.as_ptr()), index, &mut dm) }.as_bool();
    found.then_some(Resolution {
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
    })
}

/// Every monitor that is part of the desktop, primary first.
pub fn monitors() -> Vec<Monitor> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `dd` is a valid DISPLAY_DEVICEW with its size set.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP).0 == 0 {
            continue;
        }
        let Some(current) = mode(&dd.DeviceName, ENUM_CURRENT_SETTINGS) else {
            continue;
        };
        let modes = (0..)
            .map_while(|n| mode(&dd.DeviceName, ENUM_DISPLAY_SETTINGS_MODE(n)))
            .collect();
        out.push(Monitor {
            device: String::from_utf16_lossy(&dd.DeviceName)
                .trim_end_matches('\0')
                .to_string(),
            primary: (dd.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE).0 != 0,
            current,
            modes,
        });
    }
    out.sort_by_key(|m| !m.primary);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_the_display_and_its_resolution() {
        let m = Monitor {
            device: r"\\.\DISPLAY2".into(),
            primary: false,
            current: Resolution {
                width: 2560,
                height: 1440,
            },
            modes: Vec::new(),
        };
        assert_eq!(m.label(), "Display 2 (2560 x 1440)");
    }
}
