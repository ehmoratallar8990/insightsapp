//! Health page — a full-window take on CleanMyMac's menu-bar panel: health score,
//! disk, memory, CPU, battery, network, connected devices, top processes and a
//! daily recommendation. Everything updates live once per second.

use super::app::{InsightApp, Page};
use super::theme::{self, *};
use crate::health::{self, Grade, Snapshot, SpeedTest};
use crate::storage::human;
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

pub fn fmt_rate(bps: f64) -> String {
    format!("{}/s", human(bps.max(0.0) as u64))
}

fn fmt_minutes(m: u32) -> String {
    if m >= 60 { format!("{}h {:02}m", m / 60, m % 60) } else { format!("{m}m") }
}

fn fmt_uptime(s: u64) -> String {
    let d = s / 86_400;
    let h = (s % 86_400) / 3600;
    if d > 0 { format!("up {d}d {h}h") } else { format!("up {h}h {}m", (s % 3600) / 60) }
}

fn grade_color(g: Grade) -> Color32 {
    match g {
        Grade::Excellent => GREEN,
        Grade::Good => Color32::from_rgb(110, 231, 183),
        Grade::Fair => AMBER,
        Grade::Poor => RED,
    }
}

/// Icon tile + title on the left, optional value on the right.
fn card_header(ui: &mut Ui, icon: &str, title: &str, color: Color32, value: Option<(String, Color32)>) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(9), with_alpha(color, 0.16));
        ui.painter().text(r.center(), Align2::CENTER_CENTER, icon, FontId::proportional(15.0), color);
        ui.add_space(4.0);
        ui.label(egui::RichText::new(title).size(15.5).strong().color(TEXT));
        if let Some((v, c)) = value {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(v).size(15.0).strong().color(c));
            });
        }
    });
}

/// A text-style action like CleanMyMac's "Free Up" links.
fn link_button(ui: &mut Ui, label: &str, color: Color32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(14.5), color);
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(16.0, 10.0), Sense::click());
    let hov = ui.ctx().animate_bool_with_time(resp.id.with("lh"), resp.hovered(), 0.12);
    if hov > 0.0 {
        ui.painter().rect_filled(rect, CornerRadius::same(8), with_alpha(color, 0.12 * hov));
    }
    ui.painter().galley(rect.center() - g.size() / 2.0 - vec2(0.0, hov), g, lerp_color(color, Color32::WHITE, 0.25 * hov));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

impl InsightApp {
    pub fn health_page(&mut self, ui: &mut Ui) {
        let s = self.monitor.get();
        let pal = self.pal;
        let started = self.page_changed_at;
        theme::debug_scroll(egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("health-scroll")).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Health").size(26.0).strong());
                    ui.label(egui::RichText::new("Live status of this computer — refreshed every second.").color(MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let t = ui.input(|i| i.time) as f32;
                    ui.label(egui::RichText::new(if s.ready { "Live" } else { "Sampling…" }).size(12.0).color(MUTED));
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    let pulse = 0.5 + 0.5 * (t * 3.0).sin();
                    ui.painter().circle_filled(r.center(), 3.0 + 2.5 * pulse, with_alpha(GREEN, 0.25 * pulse));
                    ui.painter().circle_filled(r.center(), 3.0, GREEN);
                });
            });
            ui.add_space(14.0);

            self.health_hero(ui, &s);
            ui.add_space(14.0);

            // ---- Card grid ----
            let w = ui.available_width();
            let cols = if w > 980.0 { 3 } else { 2 };
            let gap = 14.0;
            let cw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
            let ch = 178.0;
            type CardFn = fn(&mut InsightApp, &mut Ui, &Snapshot);
            let cards: [(&str, CardFn); 6] = [
                ("disk", InsightApp::disk_card),
                ("mem", InsightApp::memory_card),
                ("cpu", InsightApp::cpu_card),
                ("batt", InsightApp::battery_card),
                ("net", InsightApp::network_card),
                ("dev", InsightApp::devices_card),
            ];
            let rows = cards.len().div_ceil(cols);
            let (grid, _) = ui.allocate_exact_size(vec2(w, rows as f32 * ch + (rows - 1) as f32 * gap), Sense::hover());
            for (i, (id, f)) in cards.iter().enumerate() {
                let (r, c) = (i / cols, i % cols);
                let a = theme::appear(ui, started, 0.05 + i as f64 * 0.045);
                let rect = Rect::from_min_size(pos2(grid.left() + c as f32 * (cw + gap), grid.top() + r as f32 * (ch + gap) + (1.0 - a) * 12.0), vec2(cw, ch));
                let mut cu = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("hc", *id)));
                cu.multiply_opacity(a);
                theme::card_at(&mut cu, rect, ("hcard", *id), |ui| f(self, ui, &s));
            }
            ui.add_space(14.0);

            // ---- Recommendation + processes ----
            self.recommendation_card(ui, &s);
            ui.add_space(14.0);
            ui.columns(2, |cols| {
                self.process_card(&mut cols[0], "Using the most memory", &s.top_mem, true);
                self.process_card(&mut cols[1], "Using the most CPU", &s.top_cpu, false);
            });
            ui.add_space(10.0);
            let _ = pal;
        });
    }

    fn health_hero(&mut self, ui: &mut Ui, s: &Snapshot) {
        let pal = self.pal;
        let (score, grade, issues) = s.score();
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 168.0), Sense::hover());
        let p = ui.painter().clone();
        let t = ui.input(|i| i.time) as f32;
        theme::glow(&p, rect.translate(vec2(0.0, 8.0)), 20, with_alpha(pal.accent, 0.18), 30.0);
        theme::gradient_fill(ui.ctx(), &p, rect, CornerRadius::same(20), lerp_color(BG, pal.accent, 0.42), lerp_color(BG, pal.accent2, 0.22));
        // Drifting light blob.
        let blob = pos2(rect.right() - 220.0 + (t * 0.4).sin() * 40.0, rect.top() + 30.0 + (t * 0.3).cos() * 16.0);
        theme::glow(&p.with_clip_rect(rect), Rect::from_center_size(blob, vec2(220.0, 120.0)), 60, with_alpha(Color32::WHITE, 0.06), 70.0);
        p.rect_stroke(rect, CornerRadius::same(20), Stroke::new(1.0, with_alpha(Color32::WHITE, 0.10)), StrokeKind::Inside);

        let left = rect.left() + 28.0;
        let title = ui.painter().layout_job({
            let mut job = egui::text::LayoutJob::default();
            let f = FontId::proportional(30.0);
            job.append(&format!("{} Health: ", device_word(&s.model)), 0.0, egui::TextFormat::simple(f.clone(), TEXT));
            job.append(if s.ready { grade.label() } else { "…" }, 0.0, egui::TextFormat::simple(f, grade_color(grade)));
            job
        });
        p.galley(pos2(left, rect.top() + 24.0), title, TEXT);
        let mut sub = s.model.clone();
        if !s.chip.is_empty() {
            sub.push_str(&format!("  ·  {}", s.chip));
        }
        if !s.os.is_empty() {
            sub.push_str(&format!("  ·  {}", s.os));
        }
        if s.uptime > 0 {
            sub.push_str(&format!("  ·  {}", fmt_uptime(s.uptime)));
        }
        p.text(pos2(left, rect.top() + 72.0), Align2::LEFT_CENTER, sub, FontId::proportional(13.5), with_alpha(TEXT, 0.75));
        let mut y = rect.top() + 102.0;
        if s.ready && issues.is_empty() {
            p.text(pos2(left, y), Align2::LEFT_CENTER, "✔  Everything looks good — no pressure points right now.", FontId::proportional(13.0), GREEN);
        }
        for i in issues.iter().take(3) {
            p.circle_filled(pos2(left + 4.0, y), 3.0, if i.penalty >= 20 { RED } else { AMBER });
            p.text(pos2(left + 16.0, y), Align2::LEFT_CENTER, &i.text, FontId::proportional(13.0), with_alpha(TEXT, 0.85));
            y += 20.0;
        }

        // Score ring.
        let c = pos2(rect.right() - 92.0, rect.center().y);
        let f = ui.ctx().animate_value_with_time(Id::unique("health-score"), if s.ready { score as f32 / 100.0 } else { 0.0 }, 1.1);
        let col = grade_color(grade);
        theme::glow(&p, Rect::from_center_size(c, vec2(90.0, 90.0)), 45, with_alpha(col, 0.18), 40.0);
        theme::ring(&p, c, 52.0, 10.0, f, lerp_color(col, pal.accent2, 0.35), col);
        p.text(c - vec2(0.0, 6.0), Align2::CENTER_CENTER, format!("{:.0}", f * 100.0), FontId::proportional(30.0), TEXT);
        p.text(c + vec2(0.0, 20.0), Align2::CENTER_CENTER, "score", FontId::proportional(11.5), MUTED);
        if f < 0.999 * score as f32 / 100.0 {
            ui.ctx().request_repaint();
        }
    }

    fn disk_card(&mut self, ui: &mut Ui, s: &Snapshot) {
        let free = if s.disk_total > 0 { s.disk_available as f32 / s.disk_total as f32 } else { 1.0 };
        let low = free < 0.10;
        card_header(ui, "💾", if s.disk_name.is_empty() { "Startup disk" } else { &s.disk_name }, self.pal.accent2, Some((format!("{:.0}%", (1.0 - free) * 100.0), if low { RED } else { MUTED })));
        ui.add_space(8.0);
        ui.label(egui::RichText::new(format!("Available: {}", human(s.disk_available))).size(14.5).color(if low { RED } else { MUTED }));
        ui.label(egui::RichText::new(format!("of {} total", human(s.disk_total))).size(12.0).color(FAINT));
        ui.add_space(10.0);
        let pal = if low { Palette { accent: AMBER, accent2: RED } } else { self.pal };
        theme::progress_bar(ui, Some(1.0 - free), pal, 8.0);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            if link_button(ui, "Free Up", self.pal.accent2).clicked() {
                self.set_page(ui, Page::Cleanup);
                self.cleanup.tab = 0;
                if self.cleanup.cats.is_none() {
                    self.cleanup.start_scan(ui.ctx());
                }
            }
        });
    }

    fn memory_card(&mut self, ui: &mut Ui, s: &Snapshot) {
        let col = if s.mem_pressure > 80.0 { RED } else if s.mem_pressure > 60.0 { AMBER } else { GREEN };
        card_header(ui, "💻", "Memory", self.pal.accent, Some((format!("{} / {}", human(s.mem_used), human(s.mem_total)), MUTED)));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Pressure:").size(14.5).color(MUTED));
            ui.label(egui::RichText::new(format!("{:.0}%", s.mem_pressure)).size(14.5).strong().color(col));
            if s.swap_used > 0 {
                ui.label(egui::RichText::new(format!("· swap {}", human(s.swap_used))).size(12.0).color(FAINT));
            }
        });
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
        theme::sparkline(ui.painter(), r.shrink2(vec2(0.0, 4.0)), &s.mem_hist, 100.0, col);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            let busy = self.mem_job.running();
            if busy {
                theme::spinner(ui, 18.0, self.pal.accent2);
            } else if link_button(ui, "Free Up", self.pal.accent2).on_hover_text("Release inactive memory (asks for your password)").clicked() {
                let msgs = self.bg_msgs.clone();
                self.mem_job.start(ui.ctx(), move || {
                    let m = match health::free_memory() {
                        Ok(()) => "Inactive memory released".to_string(),
                        Err(e) => format!("Couldn't free memory: {e}"),
                    };
                    msgs.lock().push(m);
                });
            }
        });
    }

    fn cpu_card(&mut self, ui: &mut Ui, s: &Snapshot) {
        let temp = s.cpu_temp.map(|t| (format!("{:.0} °C", t), if t > 90.0 { RED } else if t > 75.0 { AMBER } else { GREEN }));
        card_header(ui, "⚙", "CPU", AMBER, temp);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Load:").size(14.5).color(MUTED));
            ui.label(egui::RichText::new(format!("{:.0}%", s.cpu)).size(14.5).strong().color(TEXT));
            if s.load.0 > 0.0 {
                ui.label(egui::RichText::new(format!("· avg {:.2} {:.2} {:.2}", s.load.0, s.load.1, s.load.2)).size(12.0).color(FAINT));
            }
        });
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
        theme::sparkline(ui.painter(), r.shrink2(vec2(0.0, 4.0)), &s.cpu_hist, 100.0, AMBER);
        // Per-core bars.
        let n = s.cores.len().min(32);
        if n > 0 {
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
            let bw = (r.width() - (n as f32 - 1.0) * 3.0) / n as f32;
            for (i, c) in s.cores.iter().take(n).enumerate() {
                let x = r.left() + i as f32 * (bw + 3.0);
                let bg = Rect::from_min_size(pos2(x, r.top()), vec2(bw, r.height()));
                ui.painter().rect_filled(bg, CornerRadius::same(2), SURFACE3);
                let v = ui.ctx().animate_value_with_time(Id::unique(("core", i)), c / 100.0, 0.6);
                let h = (r.height() * v).max(1.0);
                let fill = Rect::from_min_max(pos2(x, r.bottom() - h), pos2(x + bw, r.bottom()));
                ui.painter().rect_filled(fill, CornerRadius::same(2), lerp_color(GREEN, AMBER, v));
            }
        }
    }

    fn battery_card(&mut self, ui: &mut Ui, s: &Snapshot) {
        let Some(b) = &s.battery else {
            card_header(ui, "🔌", "Power", GREEN, None);
            ui.add_space(8.0);
            ui.label(egui::RichText::new("No battery — running on mains power.").size(14.0).color(MUTED));
            return;
        };
        let col = if b.percent < 15.0 { RED } else if b.percent < 30.0 { AMBER } else { GREEN };
        card_header(ui, if b.charging { "⚡" } else { "🔋" }, "Battery", col, Some((format!("{:.0}%", b.percent), TEXT)));
        ui.add_space(6.0);
        let status = match (b.charging, b.minutes) {
            (true, Some(m)) => format!("Charging · {} to full", fmt_minutes(m)),
            (true, None) => "Charging".into(),
            (false, Some(m)) => format!("{} remaining", fmt_minutes(m)),
            (false, None) if b.plugged || b.percent >= 99.0 => "Fully charged".into(),
            (false, None) => "Calculating…".into(),
        };
        ui.label(egui::RichText::new(status).size(14.5).color(MUTED));
        ui.add_space(10.0);
        // Battery glyph with animated fill.
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width().min(220.0), 30.0), Sense::hover());
        let body = Rect::from_min_max(r.min, pos2(r.right() - 8.0, r.bottom()));
        let p = ui.painter();
        p.rect_stroke(body, CornerRadius::same(8), Stroke::new(1.5, with_alpha(TEXT, 0.5)), StrokeKind::Inside);
        p.rect_filled(Rect::from_min_max(pos2(body.right() + 2.0, body.center().y - 6.0), pos2(r.right(), body.center().y + 6.0)), CornerRadius::same(2), with_alpha(TEXT, 0.5));
        let f = ui.ctx().animate_value_with_time(Id::unique("batt-fill"), b.percent / 100.0, 0.9);
        let inner = body.shrink(4.0);
        let fill = Rect::from_min_size(inner.min, vec2(inner.width() * f, inner.height()));
        theme::gradient_fill(ui.ctx(), p, fill, CornerRadius::same(5), lerp_color(col, Color32::WHITE, 0.15), col);
        if b.charging {
            let t = ui.input(|i| i.time) as f32;
            let x = inner.left() + (t * 0.5).fract() * inner.width();
            p.with_clip_rect(fill).rect_filled(Rect::from_min_size(pos2(x, inner.top()), vec2(18.0, inner.height())), CornerRadius::ZERO, with_alpha(Color32::WHITE, 0.25));
            ui.ctx().request_repaint();
        }
        ui.add_space(8.0);
        let mut bits = Vec::new();
        if let Some(h) = b.health {
            bits.push(format!("Health {h:.0}%"));
        }
        if let Some(c) = b.cycles {
            bits.push(format!("{c} cycles"));
        }
        if let Some(t) = b.temp_c {
            bits.push(format!("{t:.0} °C"));
        }
        ui.label(egui::RichText::new(bits.join("  ·  ")).size(12.0).color(FAINT));
    }

    fn network_card(&mut self, ui: &mut Ui, s: &Snapshot) {
        card_header(ui, "📶", "Network", self.pal.accent2, None);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("↑ {}", fmt_rate(s.net_up))).size(14.5).color(TEXT));
            ui.add_space(14.0);
            ui.label(egui::RichText::new(format!("↓ {}", fmt_rate(s.net_down))).size(14.5).color(TEXT));
        });
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
        let max = s.down_hist.iter().chain(s.up_hist.iter()).cloned().fold(1024.0, f32::max);
        theme::sparkline(ui.painter(), r.shrink2(vec2(0.0, 3.0)), &s.up_hist, max, with_alpha(AMBER, 0.9));
        theme::sparkline(ui.painter(), r.shrink2(vec2(0.0, 3.0)), &s.down_hist, max, self.pal.accent2);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &s.speed {
                        SpeedTest::Running(m) => {
                            ui.label(egui::RichText::new(format!("{m:.0} Mbps")).size(13.5).color(self.pal.accent2));
                            theme::spinner(ui, 16.0, self.pal.accent2);
                        }
                        st => {
                            if link_button(ui, if matches!(st, SpeedTest::Idle) { "Test Speed" } else { "Test Again" }, self.pal.accent2)
                                .on_hover_text("Downloads up to 50 MB from speed.cloudflare.com")
                                .clicked()
                            {
                                self.monitor.speed_test();
                            }
                            match st {
                                SpeedTest::Done { down_mbps, latency_ms } => {
                                    ui.label(egui::RichText::new(format!("{down_mbps:.0} Mbps · {latency_ms:.0} ms")).size(13.5).strong().color(GREEN));
                                }
                                SpeedTest::Failed(e) => {
                                    ui.label(egui::RichText::new("offline").size(12.5).color(RED)).on_hover_text(e);
                                }
                                _ => {}
                            }
                        }
                    }
                });
            });
        });
    }

    fn devices_card(&mut self, ui: &mut Ui, s: &Snapshot) {
        card_header(ui, "🔗", "Connected Devices", GREEN, None);
        ui.add_space(6.0);
        if s.volumes.is_empty() && s.peripherals.is_empty() {
            ui.label(egui::RichText::new("No external drives or Bluetooth devices.").size(13.5).color(MUTED));
            return;
        }
        let mut eject: Option<String> = None;
        egui::ScrollArea::vertical().max_height(ui.available_height()).id_salt("devs").show(ui, |ui| {
            for v in &s.volumes {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("💽").size(14.0));
                    ui.label(egui::RichText::new(&v.name).size(14.0).color(TEXT)).on_hover_text(format!("{} · {} free of {}", v.mount, human(v.available), human(v.total)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if link_button(ui, "⏏", self.pal.accent2).on_hover_text("Eject").clicked() {
                            eject = Some(v.mount.clone());
                        }
                        ui.label(egui::RichText::new(human(v.total)).size(12.0).color(FAINT));
                    });
                });
            }
            for d in &s.peripherals {
                ui.horizontal(|ui| {
                    let icon = match d.kind.to_ascii_lowercase().as_str() {
                        k if k.contains("head") || k.contains("ear") => "🎧",
                        k if k.contains("keyboard") => "⌨",
                        k if k.contains("mouse") || k.contains("trackpad") => "🖱",
                        k if k.contains("phone") => "📱",
                        k if k.contains("speaker") => "🔊",
                        _ => "📡",
                    };
                    ui.label(egui::RichText::new(icon).size(14.0));
                    ui.label(egui::RichText::new(&d.name).size(14.0).color(TEXT));
                    if let Some(b) = d.battery {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let c = if b < 20 { RED } else { TEXT };
                            ui.label(egui::RichText::new(format!("{b}%")).size(14.0).strong().color(c));
                        });
                    }
                });
            }
        });
        if let Some(m) = eject {
            let msgs = self.bg_msgs.clone();
            std::thread::spawn(move || {
                let msg = match health::eject(&m) {
                    Ok(()) => format!("Ejected {m}"),
                    Err(e) => format!("Couldn't eject: {e}"),
                };
                msgs.lock().push(msg);
            });
        }
    }

    fn recommendation_card(&mut self, ui: &mut Ui, s: &Snapshot) {
        let pal = self.pal;
        let junk: u64 = self.cleanup.cats.as_ref().map(|c| c.iter().filter(|c| c.safe).map(|c| c.bytes()).sum()).unwrap_or(0);
        let free = if s.disk_total > 0 { s.disk_available as f64 / s.disk_total as f64 } else { 1.0 };
        // (icon, title, body, button, action)
        let (icon, title, body, button, action): (&str, String, String, &str, u8) = if junk > 500 << 20 {
            ("✨", format!("Free {} of junk", human(junk)), "Caches, logs and browser data your apps will rebuild on their own.".into(), "Review & clean", 0)
        } else if free < 0.15 {
            (
                "💾",
                format!("Your disk is {:.0}% full", (1.0 - free) * 100.0),
                "Scan for junk and old project build folders, then look at the largest files.".into(),
                "Scan for junk",
                0,
            )
        } else if s.mem_pressure > 75.0 {
            ("💻", "Memory is under pressure".into(), "Quit an app you are not using from the list below, or release inactive memory.".into(), "Free memory", 1)
        } else if s.uptime > 10 * 86_400 {
            ("🔄", format!("Running for {} days", s.uptime / 86_400), "A restart clears leaked memory and applies pending system updates.".into(), "Run maintenance", 2)
        } else {
            ("✨", "Give your system a quick tune-up".into(), "Flush DNS, rebuild Quick Look thumbnails and Launch Services in one click.".into(), "Run maintenance", 2)
        };
        ui.label(egui::RichText::new("Today's Recommendation").size(18.0).strong().color(TEXT));
        ui.add_space(8.0);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 104.0), Sense::hover());
        let a = theme::appear(ui, self.page_changed_at, 0.35);
        let rect = rect.translate(vec2(0.0, (1.0 - a) * 10.0));
        let mut cu = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt("rec"));
        cu.multiply_opacity(a);
        theme::card_at(&mut cu, rect, "rec-card", |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
                theme::glow(ui.painter(), r, 16, with_alpha(pal.accent, 0.35), 18.0);
                theme::gradient_fill(ui.ctx(), ui.painter(), r, CornerRadius::same(16), pal.accent, pal.accent2);
                ui.painter().text(r.center(), Align2::CENTER_CENTER, icon, FontId::proportional(26.0), Color32::WHITE);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(title).size(16.5).strong().color(TEXT));
                    ui.label(egui::RichText::new(body).size(13.5).color(MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, button, pal).clicked() {
                        match action {
                            0 => {
                                self.set_page(ui, Page::Cleanup);
                                self.cleanup.tab = 0;
                                if self.cleanup.cats.is_none() {
                                    self.cleanup.start_scan(ui.ctx());
                                }
                            }
                            1 => {
                                let msgs = self.bg_msgs.clone();
                                self.mem_job.start(ui.ctx(), move || {
                                    let m = match health::free_memory() {
                                        Ok(()) => "Inactive memory released".to_string(),
                                        Err(e) => format!("Couldn't free memory: {e}"),
                                    };
                                    msgs.lock().push(m);
                                });
                            }
                            _ => {
                                self.set_page(ui, Page::Cleanup);
                                self.cleanup.tab = 1;
                            }
                        }
                    }
                });
            });
        });
    }

    fn process_card(&mut self, ui: &mut Ui, title: &str, procs: &[health::Proc], by_mem: bool) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).size(15.0).strong());
            ui.add_space(6.0);
            if procs.is_empty() {
                ui.label(egui::RichText::new("Sampling…").color(FAINT));
                return;
            }
            let max = if by_mem { procs.first().map(|p| p.mem as f32).unwrap_or(1.0) } else { 100.0 };
            for p in procs.iter().take(5) {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
                let hov = ui.ctx().animate_bool_with_time(Id::unique(("proc-h", title, p.pid)), resp.hovered(), 0.12);
                let painter = ui.painter();
                if hov > 0.0 {
                    painter.rect_filled(rect, CornerRadius::same(8), with_alpha(Color32::WHITE, 0.03 * hov));
                }
                let frac = if by_mem { p.mem as f32 / max.max(1.0) } else { (p.cpu / 100.0).clamp(0.0, 1.0) };
                let bar = Rect::from_min_size(pos2(rect.left() + 4.0, rect.bottom() - 4.0), vec2((rect.width() - 8.0) * frac, 2.0));
                painter.rect_filled(bar, CornerRadius::same(1), with_alpha(self.pal.accent2, 0.6));
                painter.text(pos2(rect.left() + 6.0, rect.center().y - 1.0), Align2::LEFT_CENTER, &p.name, FontId::proportional(13.0), TEXT);
                let v = if by_mem { human(p.mem) } else { format!("{:.1}%", p.cpu) };
                let quit = Rect::from_center_size(pos2(rect.right() - 26.0, rect.center().y - 1.0), vec2(44.0, 22.0));
                painter.text(pos2(rect.right() - 6.0 - 52.0 * hov, rect.center().y - 1.0), Align2::RIGHT_CENTER, v, FontId::proportional(12.5), MUTED);
                if hov > 0.05 {
                    let r = ui.interact(quit, Id::unique(("quit", title, p.pid)), Sense::click());
                    let qh = r.hovered();
                    ui.painter().rect(quit, CornerRadius::same(6), with_alpha(if qh { RED } else { SURFACE3 }, hov), Stroke::NONE, StrokeKind::Inside);
                    ui.painter().text(quit.center(), Align2::CENTER_CENTER, "Quit", FontId::proportional(11.5), with_alpha(TEXT, hov));
                    if r.clicked() {
                        let ok = health::quit_process(p.pid);
                        self.toast(ui, if ok { format!("Asked {} to quit", p.name) } else { format!("Couldn't quit {}", p.name) });
                    }
                }
            }
        });
    }
}

fn device_word(model: &str) -> &'static str {
    if model.contains("Mac") || cfg!(target_os = "macos") {
        "Mac"
    } else if cfg!(windows) {
        "PC"
    } else {
        "System"
    }
}
