//! Riptide's launcher: a window for every setting, in place of flags and environment
//! variables. See README.md in this folder.

// No console window behind the launcher in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod displays;
mod logic;
mod snow;
mod theme;

use eframe::egui;

/// The window's icon, 64 x 64 raw RGBA. `scripts/make-icon.ps1` draws it, together with the
/// icon built into the .exe.
const ICON: &[u8] = include_bytes!("../assets/icon-64.rgba");

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Riptide")
            .with_icon(egui::IconData {
                rgba: ICON.to_vec(),
                width: 64,
                height: 64,
            })
            .with_inner_size([600.0, 600.0])
            .with_resizable(false),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Riptide",
        options,
        Box::new(|cc| Ok(Box::new(app::LauncherApp::new(cc)))),
    )
}
