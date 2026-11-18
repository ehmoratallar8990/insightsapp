//! Media page: on-device CLIP model status, library analysis, label cloud,
//! natural-language image search and an animated thumbnail grid.

use super::app::{InsightApp, Page, fmt_count};
use super::storage_view::shorten_home;
use super::theme::{self, *};
use crate::filetype::Kind;
use crate::media::{Phase, clip};
use crate::search::SortKey;
use crate::storage::human;
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

impl InsightApp {
    pub fn media_page(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        self.media.prepare();
        // Pick up semantic search results.
        let (phase, device, rate, searching, search) = {
            let sh = self.media.shared.lock();
            (sh.phase.clone(), sh.device.clone(), sh.rate, sh.searching, sh.search.clone())
        };
        if let Some((q, res)) = search {
            if q == self.media_query && self.media_label.is_none() && self.media_grid_title != format!("“{q}”") {
                self.media_grid = res.into_iter().map(|r| r.0).collect();
                self.media_grid_title = format!("“{q}”");
                self.media_grid_at = ui.input(|i| i.time);
            }
        }

        debug_scroll(egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("media-scroll")).show(ui, |ui| {
            ui.label(egui::RichText::new("Media").size(26.0).strong());
            ui.label(egui::RichText::new("Search photos by what's in them. Runs fully on-device — nothing leaves your computer.").color(MUTED));
            ui.add_space(14.0);

            // ---- Library stats (from the index) ----
            let stats = {
                let idx = self.engine.index.read();
                let mut s = [(0u64, 0u64); 3];
                for e in &idx.entries {
                    if e.is_deleted() || e.is_dir() {
                        continue;
                    }
                    let slot = match e.kind() {
                        Kind::Image => 0,
                        Kind::Video => 1,
                        Kind::Audio => 2,
                        _ => continue,
                    };
                    s[slot].0 += 1;
                    s[slot].1 += e.alloc;
                }
                s
            };
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                for (i, (k, (n, b))) in [Kind::Image, Kind::Video, Kind::Audio].iter().zip(stats).enumerate() {
                    let resp = theme::card()
                        .show(ui, |ui| {
                            ui.set_width(200.0);
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(42.0, 42.0), Sense::hover());
                                ui.painter().rect_filled(r, CornerRadius::same(12), with_alpha(kind_color(*k), 0.15));
                                ui.painter().text(r.center(), Align2::CENTER_CENTER, k.icon(), FontId::proportional(20.0), kind_color(*k));
                                ui.vertical(|ui| {
                                    let shown = theme::animated_number(ui, Id::unique(("mstat", i)), n as f64);
                                    ui.label(egui::RichText::new(fmt_count(shown as u64)).size(20.0).strong());
                                    ui.label(egui::RichText::new(format!("{} · {}", k.label(), human(b))).size(12.0).color(MUTED));
                                });
                            });
                        })
                        .response
                        .interact(Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        self.kind = Some(*k);
                        self.query.clear();
                        self.sort = SortKey::Modified;
                        self.set_page(ui, Page::Search);
                    }
                }
                let analyzed = self.media.store.read().records.len();
                theme::card().show(ui, |ui| {
                    ui.set_width(200.0);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(42.0, 42.0), Sense::hover());
                        theme::gradient_fill(ui.ctx(), ui.painter(), r, CornerRadius::same(12), pal.accent, pal.accent2);
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, "✨", FontId::proportional(18.0), Color32::WHITE);
                        ui.vertical(|ui| {
                            let shown = theme::animated_number(ui, Id::unique("mstat-ai"), analyzed as f64);
                            ui.label(egui::RichText::new(fmt_count(shown as u64)).size(20.0).strong());
                            ui.label(egui::RichText::new("photos understood").size(12.0).color(MUTED));
                        });
                    });
                });
            });
            ui.add_space(14.0);

            // ---- Model card ----
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    theme::section_title(ui, "Media intelligence", pal);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (txt, col) = match &phase {
                            Phase::Idle if !clip::is_downloaded() => ("not installed".to_string(), FAINT),
                            Phase::Idle => ("idle".to_string(), FAINT),
                            Phase::Downloading { .. } => ("downloading".into(), pal.accent2),
                            Phase::Loading => ("loading model".into(), pal.accent2),
                            Phase::Analyzing { .. } => ("analyzing".into(), pal.accent2),
                            Phase::Ready => (format!("ready · {device}"), GREEN),
                            Phase::Error(_) => ("error".into(), RED),
                        };
                        ui.label(egui::RichText::new(txt).size(12.0).color(col));
                    });
                });
                ui.label(
                    egui::RichText::new(
                        "OpenAI CLIP ViT-B/32 (MIT license) running natively in Rust with candle. It assigns labels like \
                         “beach”, “receipt”, “screenshot” or “dog” and lets you search photos in plain language.",
                    )
                    .size(12.5)
                    .color(MUTED),
                );
                ui.add_space(10.0);
                match &phase {
                    Phase::Downloading { done, total } => {
                        ui.label(egui::RichText::new(format!("Downloading model · {} of {}", human(*done), if *total > 0 { human(*total) } else { "?".into() })).size(12.5));
                        theme::progress_bar(ui, if *total > 0 { Some(*done as f32 / *total as f32) } else { None }, pal, 8.0);
                    }
                    Phase::Loading => {
                        ui.label(egui::RichText::new("Loading model…").size(12.5));
                        theme::progress_bar(ui, None, pal, 8.0);
                    }
                    Phase::Analyzing { done, total } => {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("Analyzing photos · {} of {}", fmt_count(*done as u64), fmt_count(*total as u64))).size(12.5));
                            if rate > 0.0 {
                                let eta = (*total - *done) as f32 / rate;
                                ui.label(egui::RichText::new(format!("{rate:.0} img/s · ~{}", fmt_eta(eta))).size(12.0).color(FAINT));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::ghost_button(ui, "Stop").clicked() {
                                    self.media.cancel();
                                }
                            });
                        });
                        theme::progress_bar(ui, Some(*done as f32 / (*total).max(1) as f32), pal, 8.0);
                    }
                    Phase::Error(e) => {
                        ui.label(egui::RichText::new(e).color(RED).size(12.5));
                    }
                    _ => {}
                }
                let busy = matches!(phase, Phase::Downloading { .. } | Phase::Loading | Phase::Analyzing { .. });
                if !busy {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Photos in").color(MUTED));
                        ui.add(egui::TextEdit::singleline(&mut self.settings.media_scope).desired_width(300.0));
                        let label = if clip::is_downloaded() { "Analyze photos" } else { "Download model (≈600 MB) & analyze" };
                        if theme::primary_button(ui, label, pal).clicked() {
                            let items = crate::media::candidates(&self.engine.index.read(), &self.settings.media_scope);
                            let n = items.len();
                            self.media.analyze(items);
                            self.toast(ui, format!("Queued {} photos for analysis", fmt_count(n as u64)));
                        }
                    });
                    ui.label(
                        egui::RichText::new("Already-analyzed photos are skipped; changed files are re-analyzed. Formats: JPEG, PNG, WebP, GIF, BMP, TIFF, HEIC (macOS).")
                            .size(11.5)
                            .color(FAINT),
                    );
                }
            });
            ui.add_space(14.0);

            // ---- Semantic search ----
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 50.0), Sense::hover());
            let eid = Id::unique("media-search");
            let focused = ui.memory(|m| m.has_focus(eid));
            let f = ui.ctx().animate_bool_with_time(Id::unique("msf"), focused, 0.2);
            if f > 0.0 {
                theme::glow(ui.painter(), bar.translate(vec2(0.0, 5.0)), 16, with_alpha(pal.accent2, 0.2 * f), 24.0);
            }
            ui.painter().rect(bar, CornerRadius::same(16), lerp_color(SURFACE, SURFACE2, f), Stroke::new(1.0, lerp_color(BORDER, pal.accent2, f)), StrokeKind::Inside);
            ui.painter().text(pos2(bar.left() + 22.0, bar.center().y), Align2::CENTER_CENTER, "✨", FontId::proportional(16.0), pal.accent2);
            let er = Rect::from_min_max(pos2(bar.left() + 44.0, bar.top() + 8.0), pos2(bar.right() - 120.0, bar.bottom() - 8.0));
            let resp = ui.put(
                er,
                egui::TextEdit::singleline(&mut self.media_query)
                    .id(eid)
                    .frame(egui::Frame::NONE)
                    .font(FontId::proportional(16.0))
                    .align(egui::Align2::LEFT_CENTER)
                    .hint_text(egui::RichText::new("Describe a photo…  “dog on the beach at sunset”, “whiteboard notes”, “birthday cake”").color(FAINT)),
            );
            let br = Rect::from_min_max(pos2(bar.right() - 112.0, bar.top() + 9.0), pos2(bar.right() - 10.0, bar.bottom() - 9.0));
            let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(br));
            let go = theme::primary_button(&mut bui, if searching { "Searching…" } else { "Search" }, pal).clicked();
            if (go || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))) && !self.media_query.trim().is_empty() {
                self.media_label = None;
                self.media_grid_title.clear();
                self.media.search(self.media_query.trim().to_string());
                self.media_query = self.media_query.trim().to_string();
            }
            if searching {
                ui.ctx().request_repaint();
            }
            ui.add_space(14.0);

            // ---- Label cloud ----
            let counts = self.media.store.read().label_counts();
            if !counts.is_empty() {
                theme::section_title(ui, "Labels in your library", pal);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                    for (l, c) in counts.iter().take(48) {
                        let sel = self.media_label.as_deref() == Some(*l);
                        if theme::chip(ui, &format!("{l} · {c}"), sel, pal.accent2, Id::unique(("lc", *l))).clicked() {
                            if sel {
                                self.media_label = None;
                                self.media_grid.clear();
                            } else {
                                self.media_label = Some(l.to_string());
                                self.media_grid = self.media.store.read().paths_for_label(l);
                                self.media_grid_title = format!("Label: {l}");
                                self.media_grid_at = ui.input(|i| i.time);
                            }
                        }
                    }
                });
                ui.add_space(14.0);
            } else if self.media.store.read().records.is_empty() {
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new("🖼  No photos analyzed yet").size(15.0));
                    ui.label(egui::RichText::new("Run “Analyze photos” above. Labels then also work in the main search: try  label:beach  or  type:image label:receipt.").size(12.5).color(MUTED));
                });
                ui.add_space(14.0);
            }

            // ---- Grid ----
            if !self.media_grid.is_empty() {
                ui.horizontal(|ui| {
                    theme::section_title(ui, &self.media_grid_title.clone(), pal);
                    ui.label(egui::RichText::new(format!("{} photos", fmt_count(self.media_grid.len() as u64))).size(12.0).color(FAINT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::ghost_button(ui, "Clear").clicked() {
                            self.media_grid.clear();
                            self.media_label = None;
                        }
                    });
                });
                self.thumb_grid(ui);
            }
            ui.add_space(24.0);
        });
    }

    fn thumb_grid(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let w = ui.available_width();
        let cell = 150.0;
        let gap = 10.0;
        let cols = ((w + gap) / (cell + gap)).floor().max(1.0) as usize;
        let cell_w = (w - gap * (cols as f32 - 1.0)) / cols as f32;
        let now = ui.input(|i| i.time);
        let paths: Vec<String> = self.media_grid.iter().take(600).cloned().collect();
        let rows = paths.len().div_ceil(cols);
        let clip = ui.clip_rect();
        let mut open: Option<String> = None;
        for r in 0..rows {
            let (row_rect, _) = ui.allocate_exact_size(vec2(w, cell_w), Sense::hover());
            if !clip.intersects(row_rect) {
                ui.add_space(gap - ui.spacing().item_spacing.y);
                continue; // off-screen: don't even request thumbnails
            }
            for c in 0..cols {
                let i = r * cols + c;
                let Some(path) = paths.get(i) else { break };
                let rect = Rect::from_min_size(pos2(row_rect.left() + c as f32 * (cell_w + gap), row_rect.top()), vec2(cell_w, cell_w));
                let resp = ui.interact(rect, Id::unique(("cell", path)), Sense::click());
                let h = ui.ctx().animate_bool_with_time(Id::unique(("cell-h", path)), resp.hovered(), 0.15);
                let appear = theme::ease_out_cubic(((now - self.media_grid_at - (i.min(40) as f64) * 0.018) / 0.3).clamp(0.0, 1.0) as f32);
                if appear < 1.0 {
                    ui.ctx().request_repaint();
                }
                let rr = Rect::from_center_size(rect.center() + vec2(0.0, (1.0 - appear) * 12.0), rect.size() * (0.94 + 0.06 * appear + 0.02 * h));
                let p = ui.painter();
                if h > 0.0 {
                    theme::glow(p, rr.translate(vec2(0.0, 6.0)), 14, with_alpha(pal.accent, 0.35 * h), 18.0);
                }
                p.rect_filled(rr, CornerRadius::same(12), with_alpha(SURFACE2, appear));
                match self.thumbs.get(ui.ctx(), path) {
                    Some((tex, size, fade)) => {
                        // Cover-fit crop.
                        let aspect = size.x / size.y.max(1.0);
                        let uv = if aspect > 1.0 {
                            let w = 1.0 / aspect;
                            Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
                        } else {
                            let hh = aspect;
                            Rect::from_min_max(pos2(0.0, (1.0 - hh) / 2.0), pos2(1.0, (1.0 + hh) / 2.0))
                        };
                        p.add(egui::epaint::RectShape::filled(rr, 12, with_alpha(Color32::WHITE, fade * appear)).with_texture(tex, uv));
                    }
                    None => {
                        let t = now as f32;
                        let shimmer = 0.5 + 0.5 * (t * 3.0 + i as f32 * 0.4).sin();
                        p.rect_filled(rr.shrink(1.0), CornerRadius::same(12), with_alpha(SURFACE3, 0.5 + 0.3 * shimmer));
                        ui.ctx().request_repaint();
                    }
                }
                p.rect_stroke(rr, CornerRadius::same(12), Stroke::new(1.0 + h, lerp_color(with_alpha(Color32::WHITE, 0.06), pal.accent, h)), StrokeKind::Inside);
                if h > 0.05 {
                    let labels = self.media.store.read().labels_for(path);
                    let txt = labels.iter().take(2).map(|(l, _)| *l).collect::<Vec<_>>().join(" · ");
                    let band = Rect::from_min_max(pos2(rr.left(), rr.bottom() - 26.0), rr.max);
                    p.rect_filled(band, CornerRadius { nw: 0, ne: 0, sw: 12, se: 12 }, Color32::from_black_alpha((170.0 * h) as u8));
                    p.text(band.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, txt, FontId::proportional(11.5), with_alpha(Color32::WHITE, h));
                }
                if resp.double_clicked() {
                    open = Some(path.clone());
                }
                let resp = resp.on_hover_text(shorten_home(path));
                resp.context_menu(|ui| {
                    if ui.button("↗  Open").clicked() {
                        Self::open_path(path);
                        ui.close();
                    }
                    if ui.button("📂  Reveal").clicked() {
                        Self::reveal(path);
                        ui.close();
                    }
                    if ui.button("🔍  Show in Search").clicked() {
                        self.query = format!("\"{}\"", std::path::Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                        self.kind = None;
                        self.set_page(ui, Page::Search);
                        ui.close();
                    }
                    if ui.button("🗑  Move to Trash").clicked() {
                        self.confirm_trash = Some(vec![path.clone()]);
                        ui.close();
                    }
                });
            }
            ui.add_space(gap - ui.spacing().item_spacing.y);
        }
        if let Some(p) = open {
            Self::open_path(&p);
        }
        if self.media_grid.len() > 600 {
            ui.label(egui::RichText::new(format!("Showing the top 600 of {}.", fmt_count(self.media_grid.len() as u64))).size(12.0).color(FAINT));
        }
    }
}

fn fmt_eta(secs: f32) -> String {
    let s = secs.max(0.0) as u64;
    if s < 60 {
        format!("{s}s left")
    } else if s < 3600 {
        format!("{}m left", s / 60)
    } else {
        format!("{}h {}m left", s / 3600, (s % 3600) / 60)
    }
}
