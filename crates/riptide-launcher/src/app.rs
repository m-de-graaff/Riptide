//! The launcher window: a header, tabs of settings, checks and logs on a card, and Play and
//! Quit, over a background of pixel snow.

use crate::displays::{self, Monitor};
use crate::logic::{self, LogFilter, PlayOutcome};
use crate::snow::Snow;
use crate::theme::{self, GUTTER, field};
use eframe::egui::{
    self, Align, Button, Color32, ComboBox, DragValue, Frame, Grid, Layout, Margin, RichText,
    Sense, Stroke, Vec2,
};
use riptide_config::{ConsoleMode, MissingPipeline, Settings, WindowMode};
use riptide_log::{Level as LogLevel, Line, Logger};
use riptide_preflight::{Check, Level};
use std::path::PathBuf;

const FIELD_WIDTH: f32 = 300.0;
const GRID_SPACING: [f32; 2] = [24.0, 14.0];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Game,
    Display,
    Performance,
    Checks,
    Logs,
}

pub struct LauncherApp {
    /// The settings file.
    path: PathBuf,
    /// Where the runtime's executable would be: beside the launcher.
    runtime: PathBuf,
    settings: Settings,
    /// The settings as last written to the file.
    saved: Settings,
    monitors: Vec<Monitor>,
    checks: Vec<Check>,
    tab: Tab,
    /// The game folder as typed; kept apart so a half-typed path is not thrown away.
    folder_text: String,
    /// The line above the card: the result of the last Play, or a problem with the file.
    message: Option<(Color32, String)>,
    snow: Snow,
    /// This session's log file.
    log: Logger,
    /// The session files, newest first, and the one the Logs tab shows.
    log_sessions: Vec<PathBuf>,
    log_shown: PathBuf,
    log_lines: Vec<Line>,
    log_filter: LogFilter,
}

fn level_colour(level: Level) -> Color32 {
    match level {
        Level::Ok => theme::OK,
        Level::Warning => theme::WARNING,
        Level::Blocking => theme::BLOCKING,
    }
}

/// A small filled circle in the row's text line.
fn dot(ui: &mut egui::Ui, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 14.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, colour);
}

impl LauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let path = riptide_config::default_path();
        let runtime = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("riptide.exe")))
            .unwrap_or_else(|| PathBuf::from("riptide.exe"));

        let mut log = Logger::start(&riptide_log::default_dir(), "launcher");
        log.info(format!(
            "Riptide launcher {} started.",
            env!("CARGO_PKG_VERSION")
        ));

        let mut message = None;
        let settings = match riptide_config::load(&path) {
            Ok(settings) => {
                log.info(format!("Settings: {}.", path.display()));
                settings
            }
            Err(error) => {
                let moved = match riptide_config::set_aside(&path) {
                    Ok(bad) => format!(
                        "It was moved to {} and the defaults are in use.",
                        bad.display()
                    ),
                    Err(e) => {
                        format!("It could not be moved aside ({e}); the defaults are in use.")
                    }
                };
                let text = format!("Riptide {error}. {moved}");
                log.warning(&text);
                message = Some((theme::WARNING, text));
                Settings::default()
            }
        };

        let folder_text = settings
            .game
            .folder
            .as_ref()
            .map(|f| f.display().to_string())
            .unwrap_or_default();
        let mut app = Self {
            path,
            runtime,
            saved: settings.clone(),
            settings,
            monitors: displays::monitors(),
            checks: Vec::new(),
            tab: Tab::Game,
            folder_text,
            message,
            snow: Snow::new(),
            log_shown: log.path().to_path_buf(),
            log,
            log_sessions: Vec::new(),
            log_lines: Vec::new(),
            log_filter: LogFilter::All,
        };
        app.refresh_checks();
        app.reload_log();
        app
    }

    fn refresh_checks(&mut self) {
        let machine = riptide_preflight::gather(&self.settings, &self.runtime);
        self.checks = riptide_preflight::judge(&machine, &self.settings);
    }

    /// Writes a line to this session's log, and shows it if the Logs tab is on this session.
    fn note(&mut self, level: LogLevel, text: impl Into<String>) {
        self.log.log(level, text);
        if self.log_shown == self.log.path() {
            self.reload_log();
        }
    }

    fn reload_log(&mut self) {
        self.log_sessions = riptide_log::sessions(&riptide_log::default_dir());
        self.log_lines = riptide_log::read(&self.log_shown);
    }

    /// Writes the settings if they changed. A failure is shown and tried again on the next change.
    fn save_if_changed(&mut self) {
        if self.settings == self.saved {
            return;
        }
        match riptide_config::save(&self.path, &self.settings) {
            Ok(()) => self.saved = self.settings.clone(),
            Err(error) => {
                let text = format!("Riptide {error}.");
                self.note(LogLevel::Error, &text);
                self.message = Some((theme::BLOCKING, text));
            }
        }
        self.refresh_checks();
    }

    fn play(&mut self, ctx: &egui::Context) {
        self.save_if_changed();
        self.refresh_checks();
        self.note(LogLevel::Info, "Play pressed.");
        for check in self.checks.clone() {
            let level = match check.level {
                Level::Ok => LogLevel::Info,
                Level::Warning => LogLevel::Warning,
                Level::Blocking => LogLevel::Error,
            };
            self.note(level, format!("{}: {}", check.title, check.detail));
        }
        match logic::play_outcome(&self.checks, self.runtime.is_file()) {
            PlayOutcome::Blocked => {
                self.note(LogLevel::Error, "Not started: a check is blocking.");
                self.tab = Tab::Checks;
                self.message = Some((
                    theme::BLOCKING,
                    "Fix the blocking checks below, then press Play again.".into(),
                ));
            }
            PlayOutcome::NotInstalled => {
                self.note(
                    LogLevel::Warning,
                    "Not started: the Riptide runtime is not installed.",
                );
                self.message = Some((
                    theme::WARNING,
                    "Your settings are saved. The Riptide runtime is not installed yet, so there is nothing to start.".into(),
                ));
            }
            PlayOutcome::Start => {
                match std::process::Command::new(&self.runtime)
                    .arg("run")
                    .arg("--config")
                    .arg(&self.path)
                    .arg("--log")
                    .arg(self.log.path())
                    .spawn()
                {
                    Ok(_) => {
                        self.note(
                            LogLevel::Info,
                            format!("Started {}.", self.runtime.display()),
                        );
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(e) => {
                        let text = format!("Riptide could not be started: {e}.");
                        self.note(LogLevel::Error, &text);
                        self.message = Some((theme::BLOCKING, text));
                    }
                }
            }
        }
    }

    /// The snow behind everything. It only moves while the window has focus.
    fn background(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, theme::BACKGROUND);
        let (dt, focused) = ui.input(|i| (i.stable_dt, i.focused));
        let pointer = ui
            .ctx()
            .pointer_hover_pos()
            .filter(|p| rect.contains(*p))
            .map(|p| (p.x - rect.left(), p.y - rect.top()));
        self.snow.step(
            if focused { dt } else { 0.0 },
            rect.width(),
            rect.height(),
            pointer,
        );
        self.snow.paint(ui.painter(), rect);
        if focused {
            ui.ctx().request_repaint();
        }
    }

    fn tab_button(&mut self, ui: &mut egui::Ui, tab: Tab, name: &str) {
        let selected = self.tab == tab;
        let text = RichText::new(name).color(if selected { theme::TEXT } else { theme::MUTED });
        let response = ui.add(Button::new(text).frame(false));
        if selected {
            let r = response.rect;
            ui.painter().hline(
                r.x_range().shrink(4.0),
                r.bottom() + 5.0,
                Stroke::new(1.5, theme::TEXT),
            );
        }
        if response.clicked() {
            self.tab = tab;
            if tab == Tab::Logs {
                self.reload_log();
            }
        }
    }

    fn game_tab(&mut self, ui: &mut egui::Ui) {
        Grid::new("game")
            .num_columns(2)
            .spacing(GRID_SPACING)
            .show(ui, |ui| {
                ui.label(field("Game folder"));
                ui.horizontal(|ui| {
                    let edit = egui::TextEdit::singleline(&mut self.folder_text)
                        .hint_text("The folder that holds the game")
                        .desired_width(FIELD_WIDTH - 84.0);
                    if ui.add(edit).changed() {
                        let typed = self.folder_text.trim();
                        self.settings.game.folder =
                            (!typed.is_empty()).then(|| PathBuf::from(typed));
                    }
                    if ui.button("Browse").clicked()
                        && let Some(folder) = rfd::FileDialog::new()
                            .set_title("Choose the game folder")
                            .pick_folder()
                    {
                        self.folder_text = folder.display().to_string();
                        self.settings.game.folder = Some(folder);
                    }
                });
                ui.end_row();

                ui.label(field("Console mode"));
                let console = &mut self.settings.game.console;
                let name = |mode| match mode {
                    ConsoleMode::SeriesX => "Xbox Series X",
                    ConsoleMode::SeriesS => "Xbox Series S",
                };
                ComboBox::from_id_salt("console")
                    .width(FIELD_WIDTH)
                    .selected_text(name(*console))
                    .show_ui(ui, |ui| {
                        for mode in [ConsoleMode::SeriesX, ConsoleMode::SeriesS] {
                            ui.selectable_value(console, mode, name(mode));
                        }
                    });
                ui.end_row();
            });
        ui.add_space(12.0);
        ui.label(
            RichText::new(
                "Series S mode renders at a lower resolution with smaller textures. It is faster and uses less memory.",
            )
            .color(theme::MUTED)
            .small(),
        );
    }

    fn display_tab(&mut self, ui: &mut egui::Ui) {
        let monitors = &self.monitors;
        let display = &mut self.settings.display;
        let chosen = monitors
            .iter()
            .find(|m| display.monitor.as_deref() == Some(m.device.as_str()))
            .or_else(|| monitors.first());

        Grid::new("display")
            .num_columns(2)
            .spacing(GRID_SPACING)
            .show(ui, |ui| {
                ui.label(field("Monitor"));
                let shown = match (&display.monitor, chosen) {
                    (None, Some(m)) => format!("Primary: {}", m.label()),
                    (Some(_), Some(m)) => m.label(),
                    (_, None) => "No monitor found".to_string(),
                };
                ComboBox::from_id_salt("monitor")
                    .width(FIELD_WIDTH)
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut display.monitor, None, "Primary monitor");
                        for m in monitors {
                            ui.selectable_value(
                                &mut display.monitor,
                                Some(m.device.clone()),
                                m.label(),
                            );
                        }
                    });
                ui.end_row();

                ui.label(field("Resolution"));
                let native = chosen
                    .map(|m| format!("Native ({})", m.current))
                    .unwrap_or_else(|| "Native".to_string());
                let shown = display
                    .resolution
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| native.clone());
                ComboBox::from_id_salt("resolution")
                    .width(FIELD_WIDTH)
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut display.resolution, None, native);
                        let modes = chosen.map(|m| m.modes.as_slice()).unwrap_or_default();
                        for r in logic::resolution_choices(modes) {
                            ui.selectable_value(&mut display.resolution, Some(r), r.to_string());
                        }
                    });
                ui.end_row();

                ui.label(field("Window mode"));
                let name = |mode| match mode {
                    WindowMode::Fullscreen => "Fullscreen",
                    WindowMode::Borderless => "Borderless window",
                    WindowMode::Windowed => "Windowed",
                };
                ComboBox::from_id_salt("window_mode")
                    .width(FIELD_WIDTH)
                    .selected_text(name(display.window_mode))
                    .show_ui(ui, |ui| {
                        for mode in [
                            WindowMode::Fullscreen,
                            WindowMode::Borderless,
                            WindowMode::Windowed,
                        ] {
                            ui.selectable_value(&mut display.window_mode, mode, name(mode));
                        }
                    });
                ui.end_row();

                ui.label(field("HDR"));
                ui.checkbox(&mut display.hdr, "Pass HDR10 through to an HDR display");
                ui.end_row();
            });
    }

    fn performance_tab(&mut self, ui: &mut egui::Ui) {
        let performance = &mut self.settings.performance;
        Grid::new("performance")
            .num_columns(2)
            .spacing(GRID_SPACING)
            .show(ui, |ui| {
                ui.label(field("RAM budget"));
                budget(ui, &mut performance.ram_budget_mib, 16, 8..=128);
                ui.end_row();

                ui.label(field("Video memory budget"));
                budget(ui, &mut performance.vram_budget_mib, 20, 4..=48);
                ui.end_row();

                ui.label(field("While a shader compiles"));
                let name = |choice| match choice {
                    MissingPipeline::SkipDraws => "Skip its draws (no stall, brief pop-in)",
                    MissingPipeline::Wait => "Wait for it (no pop-in, a stall)",
                };
                ComboBox::from_id_salt("missing_pipeline")
                    .width(FIELD_WIDTH)
                    .selected_text(name(performance.missing_pipeline))
                    .show_ui(ui, |ui| {
                        for choice in [MissingPipeline::SkipDraws, MissingPipeline::Wait] {
                            ui.selectable_value(
                                &mut performance.missing_pipeline,
                                choice,
                                name(choice),
                            );
                        }
                    });
                ui.end_row();

                ui.label(field("Video memory copies"));
                ui.checkbox(
                    &mut performance.promotion,
                    "Copy unchanging data to video memory (faster)",
                );
                ui.end_row();

                ui.label(field("Frame-time readout"));
                ui.checkbox(
                    &mut performance.readout,
                    "Show where each frame's time went",
                );
                ui.end_row();
            });
    }

    fn checks_tab(&mut self, ui: &mut egui::Ui) {
        Grid::new("checks")
            .num_columns(3)
            .spacing([16.0, 12.0])
            .show(ui, |ui| {
                for check in &self.checks {
                    let colour = level_colour(check.level);
                    ui.horizontal(|ui| {
                        dot(ui, colour);
                        ui.label(RichText::new(match check.level {
                            Level::Ok => "OK",
                            Level::Warning => "Warning",
                            Level::Blocking => "Blocking",
                        }));
                    });
                    ui.label(check.title);
                    ui.add(egui::Label::new(field(&check.detail)).wrap());
                    ui.end_row();
                }
            });
        ui.add_space(10.0);
        if ui.button("Check again").clicked() {
            self.refresh_checks();
        }
    }
}

impl LauncherApp {
    fn logs_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let current = self.log.path().to_path_buf();
            let label = |path: &PathBuf| {
                if *path == current {
                    "This session".to_string()
                } else {
                    logic::session_label(&path.file_name().unwrap_or_default().to_string_lossy())
                }
            };
            let before = self.log_shown.clone();
            ComboBox::from_id_salt("log_session")
                .width(160.0)
                .selected_text(label(&self.log_shown))
                .show_ui(ui, |ui| {
                    for session in &self.log_sessions {
                        ui.selectable_value(&mut self.log_shown, session.clone(), label(session));
                    }
                });
            ComboBox::from_id_salt("log_filter")
                .width(84.0)
                .selected_text(self.log_filter.name())
                .show_ui(ui, |ui| {
                    for filter in [LogFilter::All, LogFilter::Warnings, LogFilter::Errors] {
                        ui.selectable_value(&mut self.log_filter, filter, filter.name());
                    }
                });
            let mut reload = self.log_shown != before;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Open folder").clicked() {
                    let _ = std::process::Command::new("explorer")
                        .arg(riptide_log::default_dir())
                        .spawn();
                }
                if ui.button("Copy").clicked() {
                    let text: Vec<String> = self
                        .log_lines
                        .iter()
                        .filter(|line| self.log_filter.shows(line.level))
                        .map(|line| line.to_string())
                        .collect();
                    let home = std::env::var("USERPROFILE").unwrap_or_default();
                    ui.ctx()
                        .copy_text(riptide_log::redact(&text.join("\n"), &home));
                }
                reload |= ui.button("Refresh").clicked();
            });
            if reload {
                self.reload_log();
            }
        });
        ui.add_space(6.0);

        let shown: Vec<&Line> = self
            .log_lines
            .iter()
            .filter(|line| self.log_filter.shows(line.level))
            .collect();
        if shown.is_empty() {
            ui.label(field("Nothing logged at this level in this session."));
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("log_lines")
            .auto_shrink([false, true])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                for line in shown {
                    ui.horizontal_top(|ui| {
                        let (word, colour) = match line.level {
                            LogLevel::Info => ("info", theme::MUTED),
                            LogLevel::Warning => ("warn", theme::WARNING),
                            LogLevel::Error => ("error", theme::BLOCKING),
                        };
                        // The time of day; the date is in the session's name.
                        let time = line.time.get(11..19).unwrap_or("");
                        ui.label(field(time).monospace().small());
                        ui.label(
                            RichText::new(format!("{word:<5}"))
                                .color(colour)
                                .monospace()
                                .small(),
                        );
                        ui.add(egui::Label::new(RichText::new(&line.message).small()).wrap());
                    });
                }
            });
    }
}

/// A budget: automatic, or a number of GiB.
fn budget(
    ui: &mut egui::Ui,
    mib: &mut Option<u32>,
    suggested_gib: u32,
    range: std::ops::RangeInclusive<u32>,
) {
    ui.horizontal(|ui| {
        let mut automatic = mib.is_none();
        if ui.checkbox(&mut automatic, "Automatic").changed() {
            *mib = if automatic {
                None
            } else {
                Some(logic::gib_to_mib(suggested_gib))
            };
        }
        if let Some(value) = mib {
            let mut gib = logic::mib_to_gib(*value);
            if ui
                .add(DragValue::new(&mut gib).range(range).suffix(" GiB"))
                .changed()
            {
                *value = logic::gib_to_mib(gib);
            }
        }
    });
}

impl eframe::App for LauncherApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.background(ui);

        egui::Panel::top("header")
            .frame(Frame::NONE.inner_margin(Margin::symmetric(GUTTER, 14)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Riptide").size(16.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(field("Xbox Series compatibility runtime").small());
                    });
                });
            });

        egui::Panel::bottom("actions")
            .frame(Frame::NONE)
            .show_separator_line(false)
            .show(ui, |ui| {
                theme::hairline(ui);
                Frame::NONE
                    .inner_margin(Margin::symmetric(GUTTER, 14))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let worst = if self.checks.iter().any(|c| c.level == Level::Blocking) {
                                Level::Blocking
                            } else if self.checks.iter().any(|c| c.level == Level::Warning) {
                                Level::Warning
                            } else {
                                Level::Ok
                            };
                            dot(ui, level_colour(worst));
                            ui.label(field(&riptide_preflight::summary(&self.checks)).small());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let play = Button::new(
                                    RichText::new("Play").color(theme::BACKGROUND).strong(),
                                )
                                .fill(theme::TEXT)
                                .min_size(Vec2::new(92.0, 32.0));
                                if ui.add(play).clicked() {
                                    self.play(ui.ctx());
                                }
                                if ui
                                    .add(Button::new("Quit").min_size(Vec2::new(72.0, 32.0)))
                                    .clicked()
                                {
                                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            });
                        });
                    });
            });

        egui::CentralPanel::default()
            .frame(Frame::NONE.inner_margin(Margin::symmetric(GUTTER, 0)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    self.tab_button(ui, Tab::Game, "Game");
                    self.tab_button(ui, Tab::Display, "Display");
                    self.tab_button(ui, Tab::Performance, "Performance");
                    self.tab_button(ui, Tab::Checks, "Checks");
                    self.tab_button(ui, Tab::Logs, "Logs");
                });
                ui.add_space(14.0);
                if let Some((colour, text)) = &self.message {
                    ui.horizontal(|ui| {
                        dot(ui, *colour);
                        ui.label(RichText::new(text).small());
                    });
                    ui.add_space(4.0);
                }
                Frame::NONE
                    .fill(Color32::from_rgba_unmultiplied(17, 17, 17, 236))
                    .stroke(Stroke::new(1.0, theme::BORDER))
                    .corner_radius(10)
                    .inner_margin(20)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        // A long list scrolls inside the card instead of running off the window.
                        egui::ScrollArea::vertical()
                            .max_height(ui.available_height() - 24.0)
                            .show(ui, |ui| match self.tab {
                                Tab::Game => self.game_tab(ui),
                                Tab::Display => self.display_tab(ui),
                                Tab::Performance => self.performance_tab(ui),
                                Tab::Checks => self.checks_tab(ui),
                                Tab::Logs => self.logs_tab(ui),
                            });
                    });
            });

        self.save_if_changed();
    }
}
