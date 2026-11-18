//! Visual language: palette, typography, and small animated building blocks.

use crate::filetype::Kind;
use egui::{
    Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Margin, Mesh, Pos2, Rect, Response, Sense, Shadow,
    Stroke, StrokeKind, Ui, Vec2, epaint::Vertex, pos2, vec2,
};

pub const BG: Color32 = Color32::from_rgb(9, 10, 15);
pub const SIDEBAR: Color32 = Color32::from_rgb(13, 14, 20);
pub const SURFACE: Color32 = Color32::from_rgb(18, 20, 28);
pub const SURFACE2: Color32 = Color32::from_rgb(24, 27, 37);
pub const SURFACE3: Color32 = Color32::from_rgb(32, 36, 49);
pub const BORDER: Color32 = Color32::from_rgb(36, 40, 54);
pub const TEXT: Color32 = Color32::from_rgb(232, 234, 242);
pub const MUTED: Color32 = Color32::from_rgb(138, 144, 166);
pub const FAINT: Color32 = Color32::from_rgb(88, 94, 114);
pub const GREEN: Color32 = Color32::from_rgb(52, 211, 153);
pub const AMBER: Color32 = Color32::from_rgb(251, 191, 36);
pub const RED: Color32 = Color32::from_rgb(244, 63, 94);

pub const ACCENTS: [(&str, Color32, Color32); 5] = [
    ("Violet", Color32::from_rgb(124, 92, 255), Color32::from_rgb(34, 211, 238)),
    ("Ocean", Color32::from_rgb(56, 140, 255), Color32::from_rgb(45, 212, 191)),
    ("Emerald", Color32::from_rgb(16, 185, 129), Color32::from_rgb(163, 230, 53)),
    ("Sunset", Color32::from_rgb(244, 63, 94), Color32::from_rgb(251, 146, 60)),
    ("Gold", Color32::from_rgb(245, 158, 11), Color32::from_rgb(250, 204, 21)),
];

#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Color32,
    pub accent2: Color32,
}

impl Palette {
    pub fn from_index(i: usize) -> Self {
        let (_, a, b) = ACCENTS[i.min(ACCENTS.len() - 1)];
        Palette { accent: a, accent2: b }
    }
}

pub fn kind_color(k: Kind) -> Color32 {
    match k {
        Kind::Folder => Color32::from_rgb(96, 165, 250),
        Kind::Image => Color32::from_rgb(244, 114, 182),
        Kind::Video => Color32::from_rgb(167, 139, 250),
        Kind::Audio => Color32::from_rgb(52, 211, 153),
        Kind::Document => Color32::from_rgb(251, 191, 36),
        Kind::Archive => Color32::from_rgb(251, 146, 60),
        Kind::Code => Color32::from_rgb(34, 211, 238),
        Kind::App => Color32::from_rgb(248, 113, 113),
        Kind::Other => Color32::from_rgb(148, 163, 184),
    }
}

pub fn with_alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn ease_out_back(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let c1 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

/// Load a modern system UI font when available, keeping egui's fonts (incl. emoji) as fallback.
pub fn install_fonts(ctx: &egui::Context) {
    let candidates: &[&str] = &[
        #[cfg(target_os = "macos")]
        "/System/Library/Fonts/SFNS.ttf",
        #[cfg(target_os = "macos")]
        "/System/Library/Fonts/Helvetica.ttc",
        #[cfg(windows)]
        "C:\\Windows\\Fonts\\segoeui.ttf",
        #[cfg(target_os = "linux")]
        "/usr/share/fonts/truetype/inter/Inter-Regular.ttf",
        #[cfg(target_os = "linux")]
        "/usr/share/fonts/opentype/inter/Inter-Regular.otf",
        #[cfg(target_os = "linux")]
        "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
        #[cfg(target_os = "linux")]
        "/usr/share/fonts/cantarell/Cantarell-VF.otf",
        #[cfg(target_os = "linux")]
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    let mut fonts = egui::FontDefinitions::default();
    let over = std::env::var("INSIGHT_FONT").ok();
    let list: Vec<&str> = match &over {
        Some(p) => vec![p.as_str()],
        None => candidates.to_vec(),
    };
    for path in list {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("ui".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

pub fn apply_style(ctx: &egui::Context, pal: Palette) {
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.extreme_bg_color = SURFACE2;
        v.faint_bg_color = SURFACE;
        v.override_text_color = Some(TEXT);
        v.hyperlink_color = pal.accent2;
        v.selection.bg_fill = with_alpha(pal.accent, 0.45);
        v.selection.stroke = Stroke::new(1.0, pal.accent);
        v.window_corner_radius = CornerRadius::same(14);
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_shadow = Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(120) };
        v.menu_corner_radius = CornerRadius::same(10);
        let r = CornerRadius::same(8);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = r;
        }
        v.widgets.noninteractive.bg_fill = SURFACE;
        v.widgets.noninteractive.weak_bg_fill = SURFACE;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
        v.widgets.inactive.bg_fill = SURFACE2;
        v.widgets.inactive.weak_bg_fill = SURFACE2;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.hovered.bg_fill = SURFACE3;
        v.widgets.hovered.weak_bg_fill = SURFACE3;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, with_alpha(pal.accent, 0.6));
        v.widgets.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        v.widgets.active.bg_fill = with_alpha(pal.accent, 0.35);
        v.widgets.active.weak_bg_fill = with_alpha(pal.accent, 0.35);
        v.widgets.active.bg_stroke = Stroke::new(1.0, pal.accent);
        v.widgets.open.weak_bg_fill = SURFACE3;

        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(12.0, 6.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.scroll.bar_width = 6.0;
        style.spacing.scroll.floating = true;
        style.animation_time = 0.18;
        use egui::TextStyle::*;
        style.text_styles = [
            (Heading, FontId::new(24.0, FontFamily::Proportional)),
            (Body, FontId::new(14.0, FontFamily::Proportional)),
            (Button, FontId::new(14.0, FontFamily::Proportional)),
            (Small, FontId::new(11.5, FontFamily::Proportional)),
            (Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
    });
}

pub fn card() -> Frame {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
}

/// Horizontal gradient rectangle (non-rounded; clip for rounded looks).
pub fn gradient_rect(painter: &egui::Painter, rect: Rect, left: Color32, right: Color32) {
    let mut mesh = Mesh::default();
    let idx = mesh.vertices.len() as u32;
    let uv = egui::epaint::WHITE_UV;
    mesh.vertices.push(Vertex { pos: rect.left_top(), uv, color: left });
    mesh.vertices.push(Vertex { pos: rect.right_top(), uv, color: right });
    mesh.vertices.push(Vertex { pos: rect.right_bottom(), uv, color: right });
    mesh.vertices.push(Vertex { pos: rect.left_bottom(), uv, color: left });
    mesh.add_triangle(idx, idx + 1, idx + 2);
    mesh.add_triangle(idx, idx + 2, idx + 3);
    painter.add(mesh);
}

/// Fill a rounded rect with a horizontal gradient (via a cached 1-D gradient texture).
pub fn gradient_fill(ctx: &egui::Context, p: &egui::Painter, rect: Rect, radius: impl Into<CornerRadius>, a: Color32, b: Color32) {
    let id = egui::Id::unique(("grad-tex", a.to_array(), b.to_array()));
    let tex = ctx.data_mut(|d| d.get_temp::<egui::TextureHandle>(id)).unwrap_or_else(|| {
        let w = 256;
        let pixels: Vec<Color32> = (0..w).map(|i| lerp_color(a, b, i as f32 / (w - 1) as f32)).collect();
        let img = egui::ColorImage { size: [w, 1], pixels, source_size: Vec2::new(w as f32, 1.0) };
        let h = ctx.load_texture("gradient", img, egui::TextureOptions::LINEAR);
        ctx.data_mut(|d| d.insert_temp(id, h.clone()));
        h
    });
    p.add(
        egui::epaint::RectShape::filled(rect, radius, Color32::WHITE)
            .with_texture(tex.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
    );
}

/// Soft glow / shadow behind a rect.
pub fn glow(p: &egui::Painter, rect: Rect, radius: u8, color: Color32, blur: f32) {
    p.add(egui::epaint::RectShape::filled(rect, radius, color).with_blur_width(blur));
}

/// A pill-shaped gradient progress bar with an optional moving shimmer.
pub fn progress_bar(ui: &mut Ui, frac: Option<f32>, pal: Palette, height: f32) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
    let p = ui.painter();
    let r = CornerRadius::same((height / 2.0) as u8);
    p.rect_filled(rect, r, SURFACE3);
    let t = ui.input(|i| i.time) as f32;
    match frac {
        Some(f) => {
            let f = ui.ctx().animate_value_with_time(resp.id.with("pb"), f.clamp(0.0, 1.0), 0.35);
            let fill = Rect::from_min_size(rect.min, vec2((rect.width() * f).max(height), rect.height()));
            gradient_fill(ui.ctx(), p, fill, r, pal.accent, pal.accent2);
            shimmer(&p.with_clip_rect(fill.shrink2(vec2(height / 2.0, 0.0))), fill, t);
        }
        None => {
            // Indeterminate: a sliding gradient segment.
            let seg = rect.width() * 0.3;
            let x = rect.left() + ((t * 0.6).fract() * (rect.width() + seg)) - seg;
            let fill = Rect::from_min_size(pos2(x, rect.top()), vec2(seg, rect.height()));
            let clip = p.with_clip_rect(rect);
            gradient_rect(&clip, fill, with_alpha(pal.accent, 0.0), pal.accent);
            ui.ctx().request_repaint();
        }
    }
    resp
}

fn shimmer(p: &egui::Painter, rect: Rect, t: f32) {
    let w = 80.0;
    let x = rect.left() + (t * 0.8).fract() * (rect.width() + w) - w;
    let r = Rect::from_min_size(pos2(x, rect.top()), vec2(w / 2.0, rect.height()));
    gradient_rect(p, r, Color32::TRANSPARENT, Color32::from_white_alpha(40));
    let r2 = Rect::from_min_size(pos2(x + w / 2.0, rect.top()), vec2(w / 2.0, rect.height()));
    gradient_rect(p, r2, Color32::from_white_alpha(40), Color32::TRANSPARENT);
}

/// An animated chip/pill toggle.
pub fn chip(ui: &mut Ui, label: &str, selected: bool, color: Color32, id: egui::Id) -> Response {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, TEXT);
    let size = vec2(galley.size().x + 24.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let sel = ui.ctx().animate_bool_with_time(id.with("sel"), selected, 0.2);
    let hov = ui.ctx().animate_bool_with_time(id.with("hov"), resp.hovered(), 0.12);
    let p = ui.painter();
    let fill = lerp_color(lerp_color(SURFACE2, SURFACE3, hov), with_alpha(color, 0.22), sel);
    let stroke = lerp_color(BORDER, color, sel.max(hov * 0.5));
    p.rect(rect, CornerRadius::same(14), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
    let tc = lerp_color(MUTED, TEXT, sel.max(hov));
    p.galley(pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0), galley, tc);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Primary call-to-action button with gradient fill and hover lift.
pub fn primary_button(ui: &mut Ui, label: &str, pal: Palette) -> Response {
    let font = FontId::proportional(14.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, Color32::WHITE);
    let size = vec2(galley.size().x + 32.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hov = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.15);
    let press = ui.ctx().animate_bool_with_time(resp.id.with("p"), resp.is_pointer_button_down_on(), 0.08);
    let r = rect.translate(vec2(0.0, -1.5 * hov + 1.0 * press));
    let p = ui.painter();
    p.add(egui::epaint::RectShape::filled(r.translate(vec2(0.0, 4.0)), 10, with_alpha(pal.accent, 0.25 + 0.35 * hov)).with_blur_width(14.0 + 8.0 * hov));
    gradient_fill(ui.ctx(), p, r, CornerRadius::same(10), pal.accent, lerp_color(pal.accent, pal.accent2, 0.7));
    p.rect_stroke(r, CornerRadius::same(10), Stroke::new(1.0, Color32::from_white_alpha((30.0 + 40.0 * hov) as u8)), StrokeKind::Inside);
    p.galley(r.center() - galley.size() / 2.0, galley, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Subtle secondary button.
pub fn ghost_button(ui: &mut Ui, label: &str) -> Response {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, TEXT);
    let size = vec2(galley.size().x + 22.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hov = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(8),
        lerp_color(SURFACE2, SURFACE3, hov),
        Stroke::new(1.0, lerp_color(BORDER, FAINT, hov)),
        StrokeKind::Inside,
    );
    p.galley(rect.center() - galley.size() / 2.0, galley, lerp_color(MUTED, TEXT, 0.6 + 0.4 * hov));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Section title with a small gradient accent bar.
pub fn section_title(ui: &mut Ui, title: &str, pal: Palette) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(4.0, 18.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(r, CornerRadius::same(2), pal.accent);
        ui.add_space(4.0);
        ui.label(egui::RichText::new(title).size(16.0).strong().color(TEXT));
    });
    ui.add_space(6.0);
}

/// Animated number (eases toward its target).
pub fn animated_number(ui: &Ui, id: egui::Id, value: f64) -> f64 {
    ui.ctx().animate_value_with_time(id, value as f32, 0.6) as f64
}

/// Donut ring for disk usage.
pub fn donut(ui: &mut Ui, frac: f32, size: f32, color: Color32, color2: Color32, id: egui::Id) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let f = ui.ctx().animate_value_with_time(id, frac.clamp(0.0, 1.0), 0.9);
    let p = ui.painter();
    let c = rect.center();
    let r = size / 2.0 - 6.0;
    let w = 9.0;
    let n = 96;
    let pts = |from: f32, to: f32| -> Vec<Pos2> {
        let steps = ((to - from) * n as f32).ceil().max(1.0) as usize;
        (0..=steps)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2 + (from + (to - from) * i as f32 / steps as f32) * std::f32::consts::TAU;
                c + vec2(a.cos(), a.sin()) * r
            })
            .collect()
    };
    p.add(egui::Shape::line(pts(0.0, 1.0), Stroke::new(w, SURFACE3)));
    if f > 0.001 {
        let points = pts(0.0, f);
        let len = points.len();
        // Gradient along the arc via short segments.
        for i in 0..len.saturating_sub(1) {
            let col = lerp_color(color, color2, i as f32 / len as f32);
            p.line_segment([points[i], points[i + 1]], Stroke::new(w, col));
        }
        p.circle_filled(points[0], w / 2.0, color);
        p.circle_filled(points[len - 1], w / 2.0, lerp_color(color, color2, 1.0));
    }
    p.text(c, Align2::CENTER_CENTER, format!("{:.0}%", f * 100.0), FontId::proportional(15.0), TEXT);
    resp
}

/// The app mark: a faceted diamond drawn with vectors.
pub fn logo_mark(p: &egui::Painter, c: Pos2, r: f32, col: Color32) {
    let pts = vec![c + vec2(0.0, -r), c + vec2(r * 0.8, 0.0), c + vec2(0.0, r), c + vec2(-r * 0.8, 0.0)];
    p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    let inner = vec![c + vec2(0.0, -r * 0.45), c + vec2(r * 0.36, 0.0), c + vec2(0.0, r * 0.45), c + vec2(-r * 0.36, 0.0)];
    p.add(egui::Shape::convex_polygon(inner, with_alpha(Color32::BLACK, 0.18), Stroke::NONE));
}

/// Developer aid for automated screenshots: `INSIGHT_SCROLL=<px>`.
pub fn debug_scroll(sa: egui::ScrollArea) -> egui::ScrollArea {
    match std::env::var("INSIGHT_SCROLL").ok().and_then(|v| v.parse::<f32>().ok()) {
        Some(y) => sa.vertical_scroll_offset(y),
        None => sa,
    }
}

/// Fade + slide-in helper for content appearing after `since` seconds.
pub fn appear(ui: &mut Ui, started: f64, delay: f64) -> f32 {
    let now = ui.input(|i| i.time);
    let t = ((now - started - delay) / 0.28).clamp(0.0, 1.0) as f32;
    if t < 1.0 {
        ui.ctx().request_repaint();
    }
    ease_out_cubic(t)
}
