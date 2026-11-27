//! The launcher's look: near-black, monochrome, hairline borders.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontFamily, FontId, Margin, RichText, Stroke, TextStyle,
    Vec2,
    epaint::text::{FontInsert, FontPriority, InsertFontFamily},
};

pub const BACKGROUND: Color32 = Color32::from_rgb(10, 10, 10);
pub const SURFACE: Color32 = Color32::from_rgb(17, 17, 17);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(26, 26, 26);
pub const BORDER: Color32 = Color32::from_rgb(38, 38, 38);
pub const BORDER_HOVER: Color32 = Color32::from_rgb(64, 64, 64);
pub const TEXT: Color32 = Color32::from_rgb(237, 237, 237);
pub const MUTED: Color32 = Color32::from_rgb(143, 143, 143);
pub const OK: Color32 = Color32::from_rgb(62, 207, 142);
pub const WARNING: Color32 = Color32::from_rgb(245, 166, 35);
pub const BLOCKING: Color32 = Color32::from_rgb(255, 99, 99);

pub const RADIUS: u8 = 6;
/// The space between the window's edge and its content.
pub const GUTTER: i8 = 20;

/// Uses Windows' own interface font when it is there; egui's built-in font otherwise.
fn use_system_font(ctx: &egui::Context) {
    let Some(windir) = std::env::var_os("WINDIR") else {
        return;
    };
    let path = std::path::Path::new(&windir)
        .join("Fonts")
        .join("segoeui.ttf");
    if let Ok(bytes) = std::fs::read(path) {
        ctx.add_font(FontInsert::new(
            "segoe-ui",
            FontData::from_owned(bytes),
            vec![InsertFontFamily {
                family: FontFamily::Proportional,
                priority: FontPriority::Highest,
            }],
        ));
    }
}

pub fn apply(ctx: &egui::Context) {
    use_system_font(ctx);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = CornerRadius::same(RADIUS + 2);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS + 2);
    visuals.extreme_bg_color = SURFACE;
    visuals.text_edit_bg_color = Some(SURFACE);
    visuals.selection.bg_fill = Color32::from_white_alpha(28);
    visuals.selection.stroke = Stroke::new(1.0, TEXT);
    visuals.hyperlink_color = TEXT;

    let radius = CornerRadius::same(RADIUS);
    let widgets = &mut visuals.widgets;
    for (state, fill, border, text) in [
        (&mut widgets.noninteractive, SURFACE, BORDER, TEXT),
        (&mut widgets.inactive, SURFACE, BORDER, TEXT),
        (&mut widgets.hovered, SURFACE_HOVER, BORDER_HOVER, TEXT),
        (&mut widgets.active, SURFACE_HOVER, BORDER_HOVER, TEXT),
        (&mut widgets.open, SURFACE_HOVER, BORDER_HOVER, TEXT),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, border);
        state.fg_stroke = Stroke::new(1.0, text);
        state.corner_radius = radius;
    }
    ctx.set_visuals(visuals);

    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.menu_margin = Margin::same(6);
        for (text_style, size) in [
            (TextStyle::Small, 11.5),
            (TextStyle::Body, 13.5),
            (TextStyle::Button, 13.5),
            (TextStyle::Heading, 17.0),
        ] {
            style
                .text_styles
                .insert(text_style, FontId::proportional(size));
        }
    });
}

/// The text of a field's name: quieter than its value.
pub fn field(text: &str) -> RichText {
    RichText::new(text).color(MUTED)
}

/// A one-pixel line across the window.
pub fn hairline(ui: &mut egui::Ui) {
    let rect = ui.max_rect();
    let y = ui.cursor().top();
    ui.painter()
        .hline(rect.x_range(), y, Stroke::new(1.0, BORDER));
}
