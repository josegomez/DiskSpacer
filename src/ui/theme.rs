//! Colours and visual style.

use crate::model::Tree;
use egui::{Color32, CornerRadius, Stroke, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(86, 156, 255);
pub const DANGER: Color32 = Color32::from_rgb(235, 87, 87);
pub const OK: Color32 = Color32::from_rgb(80, 200, 140);
pub const WARN: Color32 = Color32::from_rgb(240, 180, 60);

/// Hand-picked, high-contrast palette for the largest file types.
pub const PALETTE: [Color32; 16] = [
    Color32::from_rgb(66, 135, 245),
    Color32::from_rgb(239, 83, 80),
    Color32::from_rgb(102, 187, 106),
    Color32::from_rgb(255, 167, 38),
    Color32::from_rgb(171, 71, 188),
    Color32::from_rgb(38, 198, 218),
    Color32::from_rgb(255, 238, 88),
    Color32::from_rgb(236, 64, 122),
    Color32::from_rgb(141, 110, 99),
    Color32::from_rgb(156, 204, 101),
    Color32::from_rgb(92, 107, 192),
    Color32::from_rgb(255, 112, 67),
    Color32::from_rgb(77, 182, 172),
    Color32::from_rgb(186, 104, 200),
    Color32::from_rgb(212, 225, 87),
    Color32::from_rgb(121, 134, 203),
];

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.panel_fill = Color32::from_rgb(24, 26, 32);
    v.window_fill = Color32::from_rgb(30, 33, 40);
    v.extreme_bg_color = Color32::from_rgb(18, 20, 24);
    v.faint_bg_color = Color32::from_rgb(30, 33, 40);
    v.selection.bg_fill = Color32::from_rgb(45, 90, 160);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(10);
    v.widgets.inactive.corner_radius = CornerRadius::same(6);
    v.widgets.hovered.corner_radius = CornerRadius::same(6);
    v.widgets.active.corner_radius = CornerRadius::same(6);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(6);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(40, 44, 54);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(52, 58, 72);
    v.striped = true;
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.interact_size.y = 24.0;
    });
}

/// Per-extension colours: the biggest types get palette colours, the rest a hashed muted hue.
pub fn ext_colors(t: &Tree) -> Vec<Color32> {
    let mut order: Vec<usize> = (0..t.exts.len()).collect();
    order.sort_by(|&a, &b| t.exts[b].alloc.cmp(&t.exts[a].alloc));
    let mut colors = vec![Color32::GRAY; t.exts.len()];
    let mut k = 0;
    for idx in order {
        let e = &t.exts[idx];
        colors[idx] = if e.name.is_empty() {
            Color32::from_rgb(150, 156, 170)
        } else if k < PALETTE.len() {
            k += 1;
            PALETTE[k - 1]
        } else {
            hashed(&e.name)
        };
    }
    colors
}

fn hashed(s: &str) -> Color32 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    let hue = (h % 360) as f32 / 360.0;
    hsv(hue, 0.35, 0.72)
}

pub fn hsv(h: f32, s: f32, v: f32) -> Color32 {
    let c = egui::ecolor::Hsva::new(h, s, v, 1.0);
    Color32::from(c)
}

/// Age heat colour: new = warm red, old = cool blue.
pub fn age_color(modified: i64, now: i64) -> Color32 {
    if modified <= 0 {
        return Color32::from_rgb(110, 110, 120);
    }
    let days = ((now - modified).max(0) as f32) / 86400.0;
    // 0d -> 0.0, ~5y -> 1.0 on a log scale
    let t = ((days + 1.0).ln() / (1826.0f32).ln()).clamp(0.0, 1.0);
    hsv(0.0 + 0.62 * t, 0.65, 0.92 - 0.25 * t)
}
