//! Places: a Google-Photos-style map of where your photos were taken.
//!
//! The base map is built in (Natural Earth country shapes rasterised once), so it
//! works offline. With "Street detail" on, OpenStreetMap tiles are fetched while
//! you zoom in, re-coloured for the dark theme and cached on disk. Only anonymous
//! tile requests leave the computer — never photo locations.

use super::app::{InsightApp, fmt_count};
use super::cleanup_view::Job;
use super::theme::{self, *};
use crate::places::{self, mercator};
use egui::{Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

const TILE_UA: &str = "Insight/0.3 (+https://github.com/ehmoratallar8990/insightsapp)";

type TileKey = (u8, u32, u32);

#[derive(Default)]
struct Tiles {
    tex: HashMap<TileKey, egui::TextureHandle>,
    order: VecDeque<TileKey>,
    inflight: HashSet<TileKey>,
    failed: HashSet<TileKey>,
    rx: Option<std::sync::mpsc::Receiver<(TileKey, Option<egui::ColorImage>)>>,
    tx: Option<std::sync::mpsc::Sender<(TileKey, Option<egui::ColorImage>)>>,
}

fn tile_path(k: TileKey) -> PathBuf {
    crate::index::data_dir().join("tiles").join(k.0.to_string()).join(k.1.to_string()).join(format!("{}.png", k.2))
}

/// Light map → dark map, keeping hues: invert luma, keep chroma (YCbCr).
fn darken(img: &image::RgbaImage) -> egui::ColorImage {
    let (w, h) = img.dimensions();
    let mut px = Vec::with_capacity((w * h) as usize);
    for p in img.pixels() {
        let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let y = 0.299 * r + 0.587 * g + 0.114 * b;
        let cb = b - y;
        let cr = r - y;
        let y2 = (255.0 - y) * 0.82 + 8.0;
        let r2 = (y2 + cr * 0.8).clamp(0.0, 255.0);
        let b2 = (y2 + cb * 0.8).clamp(0.0, 255.0);
        let g2 = ((y2 - 0.299 * r2 - 0.114 * b2) / 0.587).clamp(0.0, 255.0);
        px.push(Color32::from_rgb(r2 as u8, g2 as u8, b2 as u8));
    }
    egui::ColorImage { size: [w as usize, h as usize], pixels: px, source_size: egui::Vec2::new(w as f32, h as f32) }
}

impl Tiles {
    fn get(&mut self, ctx: &egui::Context, k: TileKey, online: bool) -> Option<egui::TextureId> {
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        if let Some(t) = self.tex.get(&k) {
            return Some(t.id());
        }
        if self.failed.contains(&k) || self.inflight.contains(&k) || self.inflight.len() >= 6 {
            return None;
        }
        let path = tile_path(k);
        if !online && !path.exists() {
            return None;
        }
        self.inflight.insert(k);
        let (tx, ctx) = (self.tx.clone()?, ctx.clone());
        std::thread::spawn(move || {
            let bytes = std::fs::read(&path).ok().or_else(|| {
                #[cfg(feature = "download")]
                {
                    let url = format!("https://tile.openstreetmap.org/{}/{}/{}.png", k.0, k.1, k.2);
                    let mut r = ureq::get(&url).header("User-Agent", TILE_UA).call().ok()?;
                    let b = r.body_mut().read_to_vec().ok()?;
                    let _ = std::fs::create_dir_all(path.parent()?);
                    let _ = std::fs::write(&path, &b);
                    Some(b)
                }
                #[cfg(not(feature = "download"))]
                None
            });
            let img = bytes.and_then(|b| image::load_from_memory(&b).ok()).map(|i| darken(&i.to_rgba8()));
            let _ = tx.send((k, img));
            ctx.request_repaint();
        });
        None
    }

    fn pump(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.rx.as_ref() else { return };
        let got: Vec<_> = rx.try_iter().collect();
        for (k, img) in got {
            self.inflight.remove(&k);
            match img {
                Some(img) => {
                    let t = ctx.load_texture(format!("tile-{}-{}-{}", k.0, k.1, k.2), img, egui::TextureOptions::LINEAR);
                    self.tex.insert(k, t);
                    self.order.push_back(k);
                    while self.order.len() > 400 {
                        if let Some(old) = self.order.pop_front() {
                            self.tex.remove(&old);
                        }
                    }
                }
                None => {
                    self.failed.insert(k);
                }
            }
        }
    }
}

pub struct PlacesUi {
    pub scope: String,
    pub online: bool,
    progress: Arc<places::Progress>,
    job: Job<usize>,
    auto_started: bool,
    /// Map center in Web-Mercator 0..1 and zoom (world width = 256·2^zoom px).
    cx: f64,
    cy: f64,
    zoom: f64,
    world_tex: Option<egui::TextureHandle>,
    tiles: Tiles,
    points: Option<(usize, Vec<(f64, f64, String)>)>,
    selected: Vec<String>,
    selected_title: String,
    fit_pending: bool,
    /// The user (or a country chip) positioned the map: don't auto-fit over it.
    moved: bool,
}

impl PlacesUi {
    pub fn new(scope: String) -> Self {
        PlacesUi {
            scope: std::env::var("INSIGHT_PLACES_SCOPE").unwrap_or(scope),
            online: true,
            progress: Arc::new(places::Progress::default()),
            job: Job::default(),
            auto_started: false,
            cx: 0.5,
            cy: 0.42,
            zoom: 1.6,
            world_tex: None,
            tiles: Tiles::default(),
            points: None,
            selected: Vec::new(),
            selected_title: String::new(),
            fit_pending: true,
            moved: false,
        }
    }
}

/// Rasterise the bundled country shapes into a Web-Mercator texture (once).
fn world_texture(ctx: &egui::Context) -> egui::TextureHandle {
    const W: usize = 2048;
    let mut px = vec![Color32::TRANSPARENT; W * W];
    let land = Color32::from_rgb(34, 40, 56);
    for (_, rings) in &places::world().countries {
        for ring in rings {
            let pts: Vec<(f32, f32)> = ring
                .iter()
                .map(|p| {
                    let (x, y) = mercator(p[1] as f64, p[0] as f64);
                    ((x * W as f64) as f32, (y * W as f64) as f32)
                })
                .collect();
            let (ymin, ymax) = pts.iter().fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
            for y in (ymin.max(0.0) as usize)..=(ymax.min(W as f32 - 1.0) as usize) {
                let fy = y as f32 + 0.5;
                let mut xs: Vec<f32> = Vec::new();
                for i in 0..pts.len() {
                    let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                    if (a.1 > fy) != (b.1 > fy) {
                        xs.push(a.0 + (fy - a.1) / (b.1 - a.1) * (b.0 - a.0));
                    }
                }
                xs.sort_by(|a, b| a.total_cmp(b));
                for pair in xs.chunks(2) {
                    if let [x0, x1] = pair {
                        for x in (x0.max(0.0) as usize)..=(x1.min(W as f32 - 1.0) as usize) {
                            px[y * W + x] = land;
                        }
                    }
                }
            }
        }
    }
    ctx.load_texture("world-base", egui::ColorImage { size: [W, W], pixels: px, source_size: egui::Vec2::new(W as f32, W as f32) }, egui::TextureOptions::LINEAR)
}

impl InsightApp {
    fn places_start_scan(&mut self, ctx: &egui::Context) {
        let items = crate::media::candidates_with(&self.engine.index.read(), &self.places.scope, |n| {
            let ext = n.rsplit('.').next().unwrap_or("");
            crate::filetype::is_decodable_image(n) || matches!(ext, "heic" | "heif" | "dng" | "cr3" | "cr2" | "nef" | "arw" | "raf" | "orf" | "rw2")
        });
        let prog = Arc::new(places::Progress::default());
        self.places.progress = prog.clone();
        self.places.job.start(ctx, move || places::scan(items, places::shared(), &prog));
    }

    pub fn places_page(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        if let Some(n) = self.places.job.poll() {
            self.places.points = None;
            self.places.fit_pending = !self.places.moved;
            self.toast(ui, format!("{} photos with a location", fmt_count(n as u64)));
        }
        if self.picker.target == 5 {
            if let Some((_, p)) = self.picker.poll() {
                self.places.scope = p;
            }
        }
        // First visit: reading locations is cheap (headers only), just do it.
        if !self.places.auto_started && self.engine.index.read().live_count() > 0 {
            self.places.auto_started = true;
            let ctx = ui.ctx().clone();
            self.places_start_scan(&ctx);
        }
        let running = self.places.job.running();
        if running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Places").size(26.0).strong());
                ui.label(egui::RichText::new("Where your photos were taken, from their GPS data. Drag to move, scroll to zoom, click a bubble to see its photos.").color(MUTED));
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Photos in").color(MUTED));
            ui.add(egui::TextEdit::singleline(&mut self.places.scope).desired_width(300.0));
            if theme::ghost_button(ui, "Choose…").clicked() {
                let s = self.places.scope.clone();
                self.picker.open(ui.ctx(), 5, "Choose a folder of photos", &s);
            }
            if running {
                theme::spinner(ui, 16.0, pal.accent2);
                ui.label(
                    egui::RichText::new(format!(
                        "Reading locations · {} / {} · {} found",
                        fmt_count(self.places.progress.done.load(Ordering::Relaxed)),
                        fmt_count(self.places.progress.total.load(Ordering::Relaxed)),
                        fmt_count(self.places.progress.found.load(Ordering::Relaxed))
                    ))
                    .color(MUTED),
                );
            } else if theme::primary_button(ui, "⟳ Read locations", pal).clicked() {
                let ctx = ui.ctx().clone();
                self.places_start_scan(&ctx);
            }
            ui.add_space(12.0);
            if theme::chip(ui, "🗺 Street detail", self.places.online, pal.accent2, Id::unique("osm-toggle"))
                .on_hover_text("Fetch OpenStreetMap tiles while zoomed in (cached). Off = fully offline map.")
                .clicked()
            {
                self.places.online = !self.places.online;
            }
            if theme::chip(ui, "🌍 Fit all", false, pal.accent, Id::unique("fit-all")).clicked() {
                self.places.fit_pending = true;
                self.places.moved = false;
            }
        });
        ui.add_space(8.0);

        // Country shortcuts.
        let countries = places::shared().read().countries();
        if !countries.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for (c, n) in countries.iter().take(14) {
                    if theme::chip(ui, &format!("{c} · {}", fmt_count(*n as u64)), self.places.selected_title == *c, pal.accent, Id::unique(("country", c))).clicked() {
                        let photos = places::shared().read().in_country(c);
                        self.places_fit(&photos);
                        self.places.moved = true;
                        self.places.selected = photos;
                        self.places.selected_title = c.clone();
                    }
                }
            });
            ui.add_space(8.0);
        }

        let h = (ui.available_height() * if self.places.selected.is_empty() { 1.0 } else { 0.62 }).max(260.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click_and_drag());
        self.map(ui, rect, &resp);

        if !self.places.selected.is_empty() {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{} · {} photos", self.places.selected_title, fmt_count(self.places.selected.len() as u64))).size(15.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::ghost_button(ui, "✖").clicked() {
                        self.places.selected.clear();
                        self.places.selected_title.clear();
                    }
                });
            });
            let photos = self.places.selected.clone();
            egui::ScrollArea::vertical().id_salt("place-photos").show(ui, |ui| {
                self.photo_strip(ui, &photos, 200, "place-photos");
            });
        }
    }

    fn places_fit(&mut self, photos: &[String]) {
        let s = places::shared().read();
        let pts: Vec<(f64, f64)> = photos.iter().filter_map(|p| s.places.get(p)).map(|pl| mercator(pl.lat, pl.lon)).collect();
        if pts.is_empty() {
            return;
        }
        let (x0, x1, y0, y1) = pts.iter().fold((1.0f64, 0.0f64, 1.0f64, 0.0f64), |a, p| (a.0.min(p.0), a.1.max(p.0), a.2.min(p.1), a.3.max(p.1)));
        self.places.cx = (x0 + x1) / 2.0;
        self.places.cy = (y0 + y1) / 2.0;
        let span = (x1 - x0).max(y1 - y0).max(0.0005);
        // Fit roughly 70% of a ~900px map.
        self.places.zoom = ((900.0 * 0.7) / (256.0 * span)).log2().clamp(1.0, 17.0);
    }

    fn map(&mut self, ui: &mut Ui, rect: Rect, resp: &egui::Response) {
        let pal = self.pal;
        let ctx = ui.ctx().clone();
        self.places.tiles.pump(&ctx);
        if self.places.world_tex.is_none() {
            self.places.world_tex = Some(world_texture(&ctx));
        }
        // Points (recomputed when the store changes).
        let n_places = places::shared().read().places.len();
        if self.places.points.as_ref().is_none_or(|p| p.0 != n_places) {
            let s = places::shared().read();
            let pts = s.places.iter().map(|(k, p)| {
                let (x, y) = mercator(p.lat, p.lon);
                (x, y, k.clone())
            });
            self.places.points = Some((n_places, pts.collect()));
        }
        // Test hook: INSIGHT_PLACES_VIEW="lat,lon,zoom" (applied once).
        if let Ok(v) = std::env::var("INSIGHT_PLACES_VIEW") {
            let done = Id::unique("places-view-hook");
            if !ctx.data(|d| d.get_temp::<bool>(done).unwrap_or(false)) && n_places > 0 {
                ctx.data_mut(|d| d.insert_temp(done, true));
                let f: Vec<f64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                if f.len() == 3 {
                    let (x, y) = mercator(f[0], f[1]);
                    self.places.cx = x;
                    self.places.cy = y;
                    self.places.zoom = f[2];
                    self.places.fit_pending = false;
                    self.places.moved = true;
                }
            }
        }
        if self.places.fit_pending && n_places > 0 {
            self.places.fit_pending = false;
            let all: Vec<String> = self.places.points.as_ref().map(|p| p.1.iter().map(|x| x.2.clone()).collect()).unwrap_or_default();
            self.places_fit(&all);
        }
        // Interaction: drag to pan, scroll to zoom around the pointer.
        let world = 256.0 * 2f64.powf(self.places.zoom);
        if resp.dragged() {
            self.places.moved = true;
            let d = resp.drag_delta();
            self.places.cx -= d.x as f64 / world;
            self.places.cy -= d.y as f64 / world;
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y + i.zoom_delta().ln() * 300.0);
            if scroll.abs() > 0.01 {
                self.places.moved = true;
                let ptr = ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.center());
                let (px, py) = (self.places.cx + (ptr.x - rect.center().x) as f64 / world, self.places.cy + (ptr.y - rect.center().y) as f64 / world);
                let nz = (self.places.zoom + scroll as f64 * 0.004).clamp(0.8, 18.0);
                let nw = 256.0 * 2f64.powf(nz);
                self.places.cx = px - (ptr.x - rect.center().x) as f64 / nw;
                self.places.cy = py - (ptr.y - rect.center().y) as f64 / nw;
                self.places.zoom = nz;
            }
        }
        self.places.cy = self.places.cy.clamp(0.0, 1.0);
        self.places.cx = self.places.cx.rem_euclid(1.0);
        let world = 256.0 * 2f64.powf(self.places.zoom);
        let to_screen = |x: f64, y: f64| -> Pos2 { pos2(rect.center().x + ((x - self.places.cx) * world) as f32, rect.center().y + ((y - self.places.cy) * world) as f32) };

        let p = ui.painter().with_clip_rect(rect);
        p.rect_filled(rect, CornerRadius::same(14), Color32::from_rgb(14, 18, 28));
        // Base map (repeat horizontally for wrap-around).
        if let Some(t) = &self.places.world_tex {
            for k in -1..=1 {
                let r = Rect::from_min_max(to_screen(k as f64, 0.0), to_screen(k as f64 + 1.0, 1.0));
                if r.intersects(rect) {
                    p.image(t.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                }
            }
        }
        // Street tiles when zoomed in.
        let z = self.places.zoom.round().clamp(0.0, 18.0) as u8;
        if self.places.zoom >= 3.0 {
            let n = 1u32 << z;
            let tile_px = world / n as f64;
            let (tx0, ty0) = (((self.places.cx * world - rect.width() as f64 / 2.0) / tile_px).floor() as i64, ((self.places.cy * world - rect.height() as f64 / 2.0) / tile_px).floor().max(0.0) as i64);
            let (tx1, ty1) = (((self.places.cx * world + rect.width() as f64 / 2.0) / tile_px).ceil() as i64, ((self.places.cy * world + rect.height() as f64 / 2.0) / tile_px).ceil().min(n as f64) as i64);
            for ty in ty0..ty1 {
                for tx in tx0..tx1 {
                    let wx = tx.rem_euclid(n as i64) as u32;
                    let key = (z, wx, ty as u32);
                    let r = Rect::from_min_max(to_screen(tx as f64 / n as f64, ty as f64 / n as f64), to_screen((tx + 1) as f64 / n as f64, (ty + 1) as f64 / n as f64));
                    if let Some(tex) = self.places.tiles.get(&ctx, key, self.places.online) {
                        p.image(tex, r.expand(0.5), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                    }
                }
            }
            if !self.places.tiles.inflight.is_empty() {
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
        }
        // Country borders.
        if self.places.zoom < 8.0 {
            for (_, rings) in &places::world().countries {
                for ring in rings {
                    let pts: Vec<Pos2> = ring.iter().map(|q| {
                        let (x, y) = mercator(q[1] as f64, q[0] as f64);
                        to_screen(x, y)
                    }).collect();
                    let bb = Rect::from_points(&pts);
                    if bb.intersects(rect) && bb.width() > 2.0 {
                        p.add(egui::Shape::line(pts, Stroke::new(0.8, with_alpha(Color32::from_rgb(120, 135, 170), 0.35))));
                    }
                }
            }
        }

        // Clusters: bucket on-screen points into 70px cells.
        let cell = 70.0f32;
        let mut buckets: HashMap<(i32, i32), (f32, f32, Vec<usize>)> = HashMap::new();
        let pts = self.places.points.as_ref().map(|p| &p.1);
        if let Some(pts) = pts {
            for (i, (x, y, _)) in pts.iter().enumerate() {
                for k in -1..=1 {
                    let sp = to_screen(*x + k as f64, *y);
                    if rect.expand(30.0).contains(sp) {
                        let b = buckets.entry(((sp.x / cell).floor() as i32, (sp.y / cell).floor() as i32)).or_insert((0.0, 0.0, Vec::new()));
                        b.0 += sp.x;
                        b.1 += sp.y;
                        b.2.push(i);
                    }
                }
            }
        }
        let mut clicked: Option<Vec<usize>> = None;
        let mut zoom_to: Option<Vec<usize>> = None;
        let mut buckets: Vec<_> = buckets.into_values().collect();
        buckets.sort_by_key(|b| b.2.len());
        for (k, (sx, sy, members)) in buckets.iter().enumerate() {
            let c = pos2(sx / members.len() as f32, sy / members.len() as f32);
            let size = if members.len() > 1 { 48.0 } else { 40.0 };
            let r = Rect::from_center_size(c, vec2(size, size));
            let resp = ui.interact(r, Id::unique(("pin", k, members[0])), Sense::click());
            let hov = ui.ctx().animate_bool_with_time(Id::unique(("pin-h", members[0])), resp.hovered(), 0.12);
            let r = r.expand(4.0 * hov);
            p.add(egui::epaint::RectShape::filled(r.translate(vec2(0.0, 3.0)), 10, Color32::from_black_alpha(120)).with_blur_width(8.0));
            p.rect_filled(r, CornerRadius::same(10), Color32::WHITE);
            let inner = r.shrink(2.5);
            let rep = &self.places.points.as_ref().unwrap().1[members[0]].2;
            if let Some((tex, sz, fade)) = self.thumbs.get(&ctx, rep) {
                let m = sz.x.min(sz.y);
                p.add(egui::epaint::RectShape::filled(inner, 8, with_alpha(Color32::WHITE, fade)).with_texture(tex, Rect::from_center_size(pos2(0.5, 0.5), vec2(m / sz.x, m / sz.y))));
            } else {
                p.rect_filled(inner, CornerRadius::same(8), pal.accent);
            }
            if members.len() > 1 {
                let badge = Rect::from_center_size(r.right_top() + vec2(-2.0, 2.0), vec2(26.0 + 6.0 * (members.len() >= 100) as i32 as f32, 18.0));
                p.rect_filled(badge, CornerRadius::same(9), pal.accent);
                p.text(badge.center(), Align2::CENTER_CENTER, fmt_count(members.len() as u64), FontId::proportional(10.5), Color32::WHITE);
            }
            if resp.clicked() {
                clicked = Some(members.clone());
            }
            if resp.double_clicked() {
                zoom_to = Some(members.clone());
            }
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        // Attribution + zoom buttons.
        if self.places.online && self.places.zoom >= 3.0 {
            p.text(rect.right_bottom() - vec2(8.0, 6.0), Align2::RIGHT_BOTTOM, "© OpenStreetMap contributors", FontId::proportional(10.5), with_alpha(TEXT, 0.6));
        } else {
            p.text(rect.right_bottom() - vec2(8.0, 6.0), Align2::RIGHT_BOTTOM, "Map: Natural Earth (offline)", FontId::proportional(10.5), with_alpha(TEXT, 0.5));
        }
        for (i, (label, dz)) in [("+", 1.0), ("−", -1.0)].iter().enumerate() {
            let b = Rect::from_min_size(rect.right_top() + vec2(-44.0, 12.0 + i as f32 * 38.0), vec2(32.0, 32.0));
            let r = ui.interact(b, Id::unique(("zoom-btn", i)), Sense::click());
            p.rect(b, CornerRadius::same(8), if r.hovered() { SURFACE3 } else { SURFACE2 }, Stroke::new(1.0, BORDER), StrokeKind::Inside);
            p.text(b.center(), Align2::CENTER_CENTER, *label, FontId::proportional(18.0), TEXT);
            if r.clicked() {
                self.places.moved = true;
                self.places.zoom = (self.places.zoom + dz).clamp(0.8, 18.0);
            }
        }
        if n_places == 0 {
            let msg = if self.places.job.running() { "Reading photo locations…" } else { "No photos with GPS data in this folder yet" };
            p.text(rect.center(), Align2::CENTER_CENTER, msg, FontId::proportional(15.0), MUTED);
        }
        let all = self.places.points.as_ref().map(|p| p.1.clone()).unwrap_or_default();
        let names = |m: &Vec<usize>| -> Vec<String> { m.iter().map(|&i| all[i].2.clone()).collect() };
        if let Some(m) = zoom_to {
            let photos = names(&m);
            self.places_fit(&photos);
        }
        if let Some(m) = clicked {
            let photos = names(&m);
            let s = places::shared().read();
            let title = photos.first().and_then(|p| s.places.get(p)).map(|pl| pl.country.clone().unwrap_or_else(|| format!("{:.3}, {:.3}", pl.lat, pl.lon))).unwrap_or_default();
            drop(s);
            self.places.selected_title = title;
            self.places.selected = photos;
        }
    }
}
