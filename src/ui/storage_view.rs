//! Storage insights page: disks, space by type, an interactive zoomable treemap,
//! cleanup suggestions, largest items and a duplicate finder.

use super::app::{InsightApp, Page, fmt_count, fmt_time};
use super::theme::{self, *};
use crate::filetype::Kind;
use crate::index::NONE;
use crate::search::SortKey;
use crate::storage::{self, Safety, StorageReport, human};
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct Tile {
    rect: Rect,
    id: u32, // NONE = aggregated "other"
    size: u64,
}

/// Squarified treemap layout (Bruls et al.).
fn squarify(items: &[(u32, u64)], rect: Rect) -> Vec<Tile> {
    let total: f64 = items.iter().map(|i| i.1 as f64).sum();
    let mut out = Vec::with_capacity(items.len());
    if total <= 0.0 || rect.width() < 1.0 || rect.height() < 1.0 {
        return out;
    }
    let area = rect.width() as f64 * rect.height() as f64;
    let scaled: Vec<(u32, u64, f64)> = items.iter().map(|&(id, s)| (id, s, s as f64 / total * area)).collect();
    let mut r = rect;
    let mut i = 0;
    while i < scaled.len() {
        let short = r.width().min(r.height()) as f64;
        let mut row_end = i + 1;
        let worst = |a: usize, b: usize| -> f64 {
            let s: f64 = scaled[a..b].iter().map(|x| x.2).sum();
            let (mx, mn) = scaled[a..b].iter().fold((0f64, f64::MAX), |(mx, mn), x| (mx.max(x.2), mn.min(x.2)));
            let s2 = s * s;
            let w2 = short * short;
            (w2 * mx / s2).max(s2 / (w2 * mn))
        };
        while row_end < scaled.len() && worst(i, row_end + 1) <= worst(i, row_end) {
            row_end += 1;
        }
        let row_sum: f64 = scaled[i..row_end].iter().map(|x| x.2).sum();
        let horizontal = r.width() >= r.height();
        if horizontal {
            let w = (row_sum / r.height() as f64) as f32;
            let mut y = r.top();
            for x in &scaled[i..row_end] {
                let h = (x.2 / w as f64) as f32;
                out.push(Tile { rect: Rect::from_min_size(pos2(r.left(), y), vec2(w, h)), id: x.0, size: x.1 });
                y += h;
            }
            r = Rect::from_min_max(pos2(r.left() + w, r.top()), r.max);
        } else {
            let h = (row_sum / r.width() as f64) as f32;
            let mut x0 = r.left();
            for x in &scaled[i..row_end] {
                let w = (x.2 / h as f64) as f32;
                out.push(Tile { rect: Rect::from_min_size(pos2(x0, r.top()), vec2(w, h)), id: x.0, size: x.1 });
                x0 += w;
            }
            r = Rect::from_min_max(pos2(r.left(), r.top() + h), r.max);
        }
        i = row_end;
    }
    out
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    Rect::from_min_max(a.min + (b.min - a.min) * t, a.max + (b.max - a.max) * t)
}

impl InsightApp {
    fn ensure_report(&mut self, ui: &Ui, force: bool) {
        if let Some(rx) = &self.report_rx {
            if let Ok(r) = rx.try_recv() {
                self.report = Some(Arc::new(r));
                self.report_rx = None;
                self.report_shown_at = ui.input(|i| i.time);
                self.treemap_path.clear();
            }
            return;
        }
        let stale = self.report.as_ref().is_none_or(|r| r.epoch != self.engine.id_epoch.load(Ordering::Relaxed));
        let ready = self.engine.index.read().live_count() > 0;
        if ready && (force || self.report.is_none() || stale) {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let eng = self.engine.clone();
            let ctx = ui.ctx().clone();
            std::thread::Builder::new()
                .name("storage-analysis".into())
                .spawn(move || {
                    let epoch = eng.id_epoch.load(Ordering::Relaxed);
                    let r = storage::analyze(&eng.index.read(), epoch);
                    let _ = tx.send(r);
                    ctx.request_repaint();
                })
                .unwrap();
            self.report_rx = Some(rx);
        }
    }

    pub fn storage_page(&mut self, ui: &mut Ui) {
        self.ensure_report(ui, false);
        let pal = self.pal;
        debug_scroll(egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("storage-scroll")).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Storage").size(26.0).strong());
                    ui.label(egui::RichText::new("See where your space goes and reclaim it safely.").color(MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.report_rx.is_none() && theme::ghost_button(ui, "⟳ Re-analyze").clicked() {
                        self.ensure_report(ui, true);
                    }
                    if let Some(r) = &self.report {
                        ui.label(egui::RichText::new(format!("analyzed in {} ms", r.elapsed_ms)).size(12.0).color(FAINT));
                    }
                });
            });
            ui.add_space(14.0);

            let Some(report) = self.report.clone() else {
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new("Analyzing storage…").size(15.0));
                    ui.add_space(8.0);
                    theme::progress_bar(ui, None, pal, 6.0);
                });
                ui.ctx().request_repaint();
                return;
            };
            let shown_at = self.report_shown_at;

            // ---- Disks + totals ----
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                for (i, d) in report.disks.iter().take(3).enumerate() {
                    theme::card().show(ui, |ui| {
                        ui.set_width(250.0);
                        ui.horizontal(|ui| {
                            let used = d.total.saturating_sub(d.available);
                            let frac = used as f32 / d.total.max(1) as f32;
                            let (a, b) = if frac > 0.9 { (AMBER, RED) } else { (pal.accent, pal.accent2) };
                            theme::donut(ui, frac, 84.0, a, b, Id::unique(("donut", i)));
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.add_space(8.0);
                                let name = if d.name.is_empty() { d.mount.clone() } else { d.name.clone() };
                                ui.label(egui::RichText::new(name).size(14.0).strong());
                                ui.label(egui::RichText::new(&d.mount).size(11.0).color(FAINT));
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new(format!("{} used", human(used))).size(13.0));
                                ui.label(egui::RichText::new(format!("{} free of {}", human(d.available), human(d.total))).size(12.0).color(MUTED));
                            });
                        });
                    });
                }
                theme::card().show(ui, |ui| {
                    ui.set_width(220.0);
                    ui.set_height(84.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("Indexed content").size(12.0).color(FAINT));
                        let b = theme::animated_number(ui, Id::unique("tot-bytes"), report.total_bytes as f64);
                        ui.label(egui::RichText::new(human(b as u64)).size(26.0).strong().color(pal.accent2));
                        ui.label(egui::RichText::new(format!("{} files", fmt_count(report.total_files))).size(12.5).color(MUTED));
                    });
                });
            });
            ui.add_space(14.0);

            // ---- Space by type ----
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "Space by type", pal);
                let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
                let grow = theme::appear(ui, shown_at, 0.05);
                let p = ui.painter();
                p.rect_filled(bar, CornerRadius::same(9), SURFACE3);
                let total = report.by_kind.iter().map(|k| k.1).sum::<u64>().max(1) as f32;
                let mut x = bar.left();
                let clip = p.with_clip_rect(Rect::from_min_size(bar.min, vec2(bar.width() * grow, bar.height())));
                let mut hovered: Option<(Kind, u64, u64)> = None;
                for &(k, b, c) in &report.by_kind {
                    let w = bar.width() * (b as f32 / total);
                    if w < 0.5 {
                        continue;
                    }
                    let seg = Rect::from_min_size(pos2(x, bar.top()), vec2(w, bar.height()));
                    let resp = ui.interact(seg, Id::unique(("seg", k as u8)), Sense::click());
                    let h = ui.ctx().animate_bool_with_time(Id::unique(("seg-h", k as u8)), resp.hovered(), 0.12);
                    clip.rect_filled(seg.shrink2(vec2(0.0, 2.0 * (1.0 - h))), CornerRadius::ZERO, lerp_color(kind_color(k), Color32::WHITE, 0.15 * h));
                    if resp.hovered() {
                        hovered = Some((k, b, c));
                    }
                    if resp.clicked() {
                        self.jump_to_kind(ui, k);
                    }
                    x += w;
                }
                // Round the ends.
                p.rect_stroke(bar.expand(2.0), CornerRadius::same(11), Stroke::new(4.0, SURFACE), StrokeKind::Inside);
                if let Some((k, b, c)) = hovered {
                    egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), Id::unique("seg-tip"), egui::PopupAnchor::Pointer).show(|ui| {
                        ui.label(format!("{} {} — {} in {} files", k.icon(), k.label(), human(b), fmt_count(c)));
                    });
                }
                ui.add_space(12.0);
                let cols = ((ui.available_width() / 210.0).floor() as usize).max(1);
                egui::Grid::new("kind-legend").num_columns(cols).spacing(vec2(18.0, 10.0)).show(ui, |ui| {
                    for (i, &(k, b, c)) in report.by_kind.iter().enumerate() {
                        let resp = ui
                            .horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                                ui.painter().circle_filled(r.center(), 5.0, kind_color(k));
                                ui.label(egui::RichText::new(k.label()).size(13.0));
                                ui.label(egui::RichText::new(human(b)).size(13.0).strong());
                                ui.label(egui::RichText::new(format!("{} files", fmt_count(c))).size(11.5).color(FAINT));
                            })
                            .response
                            .interact(Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("Show the largest of these in Search");
                        if resp.clicked() {
                            self.jump_to_kind(ui, k);
                        }
                        if (i + 1) % cols == 0 {
                            ui.end_row();
                        }
                    }
                });
            });
            ui.add_space(14.0);

            // ---- Treemap ----
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.treemap(ui, &report);
            });
            ui.add_space(14.0);

            // ---- Cleanup + largest ----
            let w = ui.available_width();
            let two_cols = w > 980.0;
            if two_cols {
                ui.horizontal_top(|ui| {
                    let cw = (w - 14.0) / 2.0;
                    ui.vertical(|ui| {
                        ui.set_width(cw);
                        self.cleanup_card(ui, &report);
                    });
                    ui.vertical(|ui| {
                        ui.set_width(cw);
                        self.largest_card(ui, &report);
                    });
                });
            } else {
                self.cleanup_card(ui, &report);
                ui.add_space(14.0);
                self.largest_card(ui, &report);
            }
            ui.add_space(14.0);
            self.duplicates_card(ui);
            ui.add_space(24.0);
        });
    }

    fn jump_to_kind(&mut self, ui: &Ui, k: Kind) {
        self.kind = Some(k);
        self.query.clear();
        self.sort = SortKey::Size;
        self.desc = false;
        self.set_page(ui, Page::Search);
    }

    fn treemap(&mut self, ui: &mut Ui, report: &StorageReport) {
        let pal = self.pal;
        let idx = self.engine.index.clone();
        let idx = idx.read();
        // Validate path (ids may be stale after a rescan).
        self.treemap_path.retain(|&id| (id as usize) < idx.len());
        let root = report.roots.first().copied().unwrap_or(0);
        if (root as usize) >= idx.len() {
            return;
        }
        let current = *self.treemap_path.last().unwrap_or(&root);

        // Header with breadcrumb.
        let mut go_to: Option<usize> = None;
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(4.0, 18.0), Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(2), pal.accent);
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Space map").size(16.0).strong());
            ui.add_space(10.0);
            let mut crumbs = vec![(usize::MAX, idx.name(root).to_string())];
            for (i, &id) in self.treemap_path.iter().enumerate() {
                crumbs.push((i, idx.name(id).to_string()));
            }
            let n = crumbs.len();
            for (j, (i, name)) in crumbs.into_iter().enumerate() {
                let last = j + 1 == n;
                let txt = egui::RichText::new(name).size(13.0).color(if last { TEXT } else { MUTED });
                if ui.add(egui::Label::new(txt).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    go_to = Some(if i == usize::MAX { 0 } else { i + 1 });
                }
                if !last {
                    ui.label(egui::RichText::new("›").color(FAINT));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(human(report.dir_sizes[current as usize])).size(13.0).color(pal.accent2));
            });
        });
        ui.add_space(8.0);

        let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), 380.0), Sense::hover());
        let now = ui.input(|i| i.time);
        let mut kids: Vec<(u32, u64)> = report
            .children_of(current)
            .iter()
            .map(|&c| (c, report.dir_sizes[c as usize]))
            .filter(|(_, s)| *s > 0)
            .collect();
        kids.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = kids.iter().map(|k| k.1).sum();
        // Show the biggest; fold the long tail into one tile.
        let mut shown: Vec<(u32, u64)> = Vec::new();
        let mut other = 0u64;
        for (i, k) in kids.into_iter().enumerate() {
            if i < 60 && (k.1 as f64) > total as f64 * 0.002 {
                shown.push(k);
            } else {
                other += k.1;
            }
        }
        if other > 0 {
            shown.push((NONE, other));
        }
        let tiles = squarify(&shown, area.shrink(1.0));

        // Zoom animation from the clicked tile.
        let (anim_t, from) = match self.treemap_anim {
            Some((r, start, _)) => {
                let t = ease_out_cubic(((now - start) / 0.4) as f32);
                if t >= 1.0 {
                    self.treemap_anim = None;
                } else {
                    ui.ctx().request_repaint();
                }
                (t, Some(r))
            }
            None => (1.0, None),
        };
        let map = |r: Rect| -> Rect {
            match from {
                Some(f) => {
                    // Map area-space rect into the 'from' rect, then interpolate to full size.
                    let rel_min = (r.min - area.min) / area.size();
                    let rel_max = (r.max - area.min) / area.size();
                    let small = Rect::from_min_max(f.min + rel_min * f.size(), f.min + rel_max * f.size());
                    lerp_rect(small, r, anim_t)
                }
                None => r,
            }
        };

        let p = ui.painter().with_clip_rect(area);
        p.rect_filled(area, CornerRadius::same(10), SURFACE2);
        let mut hovered: Option<&Tile> = None;
        let mut clicked: Option<(u32, Rect)> = None;
        for (i, t) in tiles.iter().enumerate() {
            let r = map(t.rect).shrink(1.5);
            if r.width() < 1.0 || r.height() < 1.0 {
                continue;
            }
            let resp = ui.interact(t.rect, Id::unique(("tile", current, i)), Sense::click());
            let h = ui.ctx().animate_bool_with_time(Id::unique(("tile-h", current, t.id)), resp.hovered(), 0.12);
            let (base, is_dir) = if t.id == NONE {
                (Color32::from_rgb(70, 76, 96), false)
            } else {
                let e = &idx.entries[t.id as usize];
                if e.is_dir() {
                    const TONES: [Color32; 6] = [
                        Color32::from_rgb(124, 92, 255),
                        Color32::from_rgb(56, 140, 255),
                        Color32::from_rgb(20, 184, 166),
                        Color32::from_rgb(236, 72, 153),
                        Color32::from_rgb(245, 158, 11),
                        Color32::from_rgb(99, 102, 241),
                    ];
                    (lerp_color(TONES[i % TONES.len()], pal.accent, 0.15), true)
                } else {
                    (kind_color(e.kind()), false)
                }
            };
            let fill = lerp_color(with_alpha(base, 0.55), with_alpha(base, 0.85), h);
            p.rect_filled(r, CornerRadius::same(6), fill);
            p.rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, with_alpha(Color32::WHITE, 0.08 + 0.25 * h)), StrokeKind::Inside);
            if r.width() > 60.0 && r.height() > 30.0 && anim_t > 0.6 {
                let name = if t.id == NONE { format!("{} smaller items", "…") } else { idx.name(t.id).to_string() };
                let job = {
                    let mut j = egui::text::LayoutJob::single_section(name, egui::TextFormat::simple(FontId::proportional(12.5), Color32::WHITE));
                    j.wrap = egui::text::TextWrapping { max_width: r.width() - 12.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                    j
                };
                let g = ui.painter().layout_job(job);
                p.galley(r.min + vec2(6.0, 5.0), g, Color32::WHITE);
                if r.height() > 46.0 {
                    p.text(r.min + vec2(6.0, 22.0), Align2::LEFT_TOP, human(t.size), FontId::proportional(11.0), with_alpha(Color32::WHITE, 0.75));
                }
                if is_dir && r.height() > 46.0 && r.width() > 40.0 {
                    p.text(r.right_top() + vec2(-8.0, 6.0), Align2::RIGHT_TOP, "📁", FontId::proportional(11.0), with_alpha(Color32::WHITE, 0.6));
                }
            }
            if resp.hovered() {
                hovered = Some(t);
            }
            if resp.clicked() && t.id != NONE && is_dir && !report.children_of(t.id).is_empty() {
                clicked = Some((t.id, t.rect));
            }
            if resp.secondary_clicked() || (resp.double_clicked() && !is_dir) {
                if t.id != NONE {
                    Self::reveal(&idx.path_string(t.id));
                }
            }
        }
        if let Some(t) = hovered {
            let text = if t.id == NONE {
                format!("Smaller items · {}", human(t.size))
            } else {
                let pct = t.size as f64 / report.dir_sizes[current as usize].max(1) as f64 * 100.0;
                format!("{}\n{} · {:.1}% of this folder\nclick folders to zoom · right-click to reveal", idx.path_string(t.id), human(t.size), pct)
            };
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), Id::unique("tile-tip"), egui::PopupAnchor::Pointer).show(|ui| {
                ui.label(text);
            });
        }
        drop(idx);
        if let Some((id, r)) = clicked {
            self.treemap_path.push(id);
            self.treemap_anim = Some((r, now, true));
        }
        if let Some(n) = go_to {
            if n < self.treemap_path.len() {
                self.treemap_path.truncate(n);
                self.treemap_anim = Some((area.shrink(area.width().min(area.height()) * 0.25), now, false));
            }
        }
        if tiles.is_empty() {
            ui.painter().text(area.center(), Align2::CENTER_CENTER, "Empty folder", FontId::proportional(14.0), FAINT);
        }
    }

    fn cleanup_card(&mut self, ui: &mut Ui, report: &StorageReport) {
        let pal = self.pal;
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let safe: u64 = report.suggestions.iter().filter(|s| s.safety == Safety::Safe).map(|s| s.bytes).sum();
            ui.horizontal(|ui| {
                theme::section_title(ui, "Cleanup suggestions", pal);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(format!("{} safely reclaimable", human(safe))).size(13.0).color(GREEN));
                });
            });
            if report.suggestions.is_empty() {
                ui.label(egui::RichText::new("Nothing obvious to clean — nice and tidy ✨").color(MUTED));
            }
            let shown_at = self.report_shown_at;
            for (i, s) in report.suggestions.iter().enumerate() {
                let a = theme::appear(ui, shown_at, 0.04 * i as f64);
                let open = self.expanded_suggestion == Some(i);
                let resp = egui::Frame::new()
                    .fill(SURFACE2)
                    .stroke(Stroke::new(1.0, if open { with_alpha(pal.accent, 0.5) } else { BORDER }))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.multiply_opacity(a);
                        let head = ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(s.icon).size(20.0));
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(&s.title).size(14.0).strong());
                                    let (badge, col) = match s.safety {
                                        Safety::Safe => ("SAFE", GREEN),
                                        Safety::Review => ("REVIEW", AMBER),
                                    };
                                    ui.label(egui::RichText::new(badge).size(10.0).color(col).background_color(with_alpha(col, 0.12)));
                                });
                                ui.set_max_width((ui.available_width() - 110.0).max(100.0));
                                ui.add(egui::Label::new(egui::RichText::new(s.detail).size(12.0).color(MUTED)).truncate());
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(egui::RichText::new(if open { "▾" } else { "▸" }).color(MUTED));
                                ui.label(egui::RichText::new(human(s.bytes)).size(15.0).strong().color(pal.accent2));
                            });
                        });
                        // Relative size bar.
                        let max = report.suggestions.first().map(|s| s.bytes).unwrap_or(1).max(1);
                        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
                        ui.painter().rect_filled(bar, CornerRadius::same(2), SURFACE3);
                        let f = (s.bytes as f32 / max as f32) * theme::appear(ui, shown_at, 0.1 + 0.04 * i as f64);
                        theme::gradient_fill(ui.ctx(), ui.painter(), Rect::from_min_size(bar.min, vec2(bar.width() * f, 4.0)), CornerRadius::same(2), pal.accent, pal.accent2);
                        if open {
                            ui.add_space(8.0);
                            for (path, bytes) in s.items.iter().take(40) {
                                ui.horizontal(|ui| {
                                    let short = shorten_home(path);
                                    ui.add(egui::Label::new(egui::RichText::new(short).size(12.0).color(MUTED)).truncate());
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("🗑").on_hover_text("Move to Trash").clicked() {
                                            self.confirm_trash = Some(vec![path.clone()]);
                                        }
                                        if ui.small_button("📂").on_hover_text("Reveal").clicked() {
                                            Self::reveal(path);
                                        }
                                        ui.label(egui::RichText::new(human(*bytes)).size(12.0));
                                    });
                                });
                            }
                            if s.items.len() > 40 {
                                ui.label(egui::RichText::new(format!("…and {} more", s.items.len() - 40)).size(12.0).color(FAINT));
                            }
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if s.safety == Safety::Safe && theme::primary_button(ui, &format!("Move all to Trash · {}", human(s.bytes)), pal).clicked() {
                                    self.confirm_trash = Some(s.items.iter().map(|i| i.0.clone()).collect());
                                }
                            });
                        }
                        head.response
                    });
                if resp.inner.interact(Sense::click()).clicked() || resp.response.interact(Sense::click()).clicked() {
                    self.expanded_suggestion = if open { None } else { Some(i) };
                }
                ui.add_space(6.0);
            }
        });
    }

    fn largest_card(&mut self, ui: &mut Ui, report: &StorageReport) {
        let pal = self.pal;
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let tab_id = Id::unique("largest-tab");
            let mut tab: u8 = ui.data(|d| d.get_temp(tab_id)).unwrap_or(0);
            ui.horizontal(|ui| {
                theme::section_title(ui, "Biggest space users", pal);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::chip(ui, "Folders", tab == 1, pal.accent, Id::unique("tab-f")).clicked() {
                        tab = 1;
                    }
                    if theme::chip(ui, "Files", tab == 0, pal.accent, Id::unique("tab-d")).clicked() {
                        tab = 0;
                    }
                });
            });
            ui.data_mut(|d| d.insert_temp(tab_id, tab));
            let items: Vec<(String, u64, String)> = if tab == 0 {
                report.largest_files.iter().take(60).map(|(p, s, m)| (p.clone(), *s, fmt_time(*m))).collect()
            } else {
                report.largest_dirs.iter().take(60).map(|(p, s, c)| (p.clone(), *s, format!("{} files", fmt_count(*c as u64)))).collect()
            };
            let max = items.first().map(|i| i.1).unwrap_or(1).max(1);
            egui::ScrollArea::vertical().max_height(520.0).id_salt(("largest", tab)).show(ui, |ui| {
                for (i, (path, size, meta)) in items.iter().enumerate() {
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
                    let h = ui.ctx().animate_bool_with_time(Id::unique(("big", tab, i)), resp.hovered(), 0.12);
                    let f = (*size as f32 / max as f32) * theme::appear(ui, self.report_shown_at, 0.02 * i as f64);
                    let p = ui.painter();
                    p.rect_filled(r.shrink2(vec2(0.0, 2.0)), CornerRadius::same(8), with_alpha(Color32::WHITE, 0.03 * h));
                    let bar = Rect::from_min_size(pos2(r.left() + 8.0, r.bottom() - 9.0), vec2((r.width() - 16.0) * f, 3.0));
                    theme::gradient_fill(ui.ctx(), p, bar, CornerRadius::same(2), with_alpha(pal.accent, 0.8), with_alpha(pal.accent2, 0.8));
                    let name = std::path::Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
                    let ng = ui.painter().layout_job({
                        let mut j = egui::text::LayoutJob::single_section(name, egui::TextFormat::simple(FontId::proportional(13.0), TEXT));
                        j.wrap = egui::text::TextWrapping { max_width: r.width() - 130.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                        j
                    });
                    p.galley(pos2(r.left() + 8.0, r.top() + 13.0 - ng.size().y / 2.0), ng, TEXT);
                    let parent = shorten_home(std::path::Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default().as_str());
                    let g = ui.painter().layout_job({
                        let mut j = egui::text::LayoutJob::single_section(parent, egui::TextFormat::simple(FontId::proportional(11.0), FAINT));
                        j.wrap = egui::text::TextWrapping { max_width: r.width() - 200.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                        j
                    });
                    p.galley(pos2(r.left() + 8.0, r.top() + 22.0), g, FAINT);
                    p.text(pos2(r.right() - 8.0, r.top() + 13.0), Align2::RIGHT_CENTER, human(*size), FontId::proportional(13.5), pal.accent2);
                    p.text(pos2(r.right() - 8.0, r.top() + 28.0), Align2::RIGHT_CENTER, meta, FontId::proportional(11.0), FAINT);
                    if resp.double_clicked() {
                        Self::reveal(path);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("📂  Reveal").clicked() {
                            Self::reveal(path);
                            ui.close();
                        }
                        if ui.button("📋  Copy path").clicked() {
                            ui.ctx().copy_text(path.clone());
                            ui.close();
                        }
                        if ui.button("🗑  Move to Trash").clicked() {
                            self.confirm_trash = Some(vec![path.clone()]);
                            ui.close();
                        }
                    });
                }
                if items.is_empty() {
                    ui.label(egui::RichText::new("No very large items found.").color(MUTED));
                }
            });
        });
    }

    pub fn start_dup_scan(&mut self, ctx: &egui::Context) {
        let prog = Arc::new(storage::DupProgress::default());
        self.dup_prog = prog.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let eng = self.engine.clone();
        let scope = self.settings.dup_scope.clone();
        let min = (self.settings.dup_min_mb as f64 * 1024.0 * 1024.0) as u64;
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("dupes".into())
            .spawn(move || {
                let cands = storage::dup_candidates(&eng.index.read(), &scope, min.max(1));
                let g = storage::find_duplicates(cands, &prog);
                let _ = tx.send(g);
                ctx.request_repaint();
            })
            .unwrap();
        self.dup_rx = Some(rx);
        self.expanded_dup = None;
    }

    fn duplicates_card(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        if let Some(rx) = &self.dup_rx {
            if let Ok(g) = rx.try_recv() {
                self.dups = Some(g);
                self.dup_rx = None;
            }
        }
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::section_title(ui, "Duplicate files", pal);
                if let Some(d) = &self.dups {
                    let wasted: u64 = d.iter().map(|g| g.wasted()).sum();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{} wasted in {} group{}", human(wasted), fmt_count(d.len() as u64), if d.len() == 1 { "" } else { "s" })).size(13.0).color(AMBER));
                    });
                }
            });
            ui.label(egui::RichText::new("Byte-identical copies, verified by full content hash. Hard links are ignored.").size(12.5).color(MUTED));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Look in").color(MUTED));
                ui.add(egui::TextEdit::singleline(&mut self.settings.dup_scope).desired_width(280.0));
                ui.label(egui::RichText::new("min size").color(MUTED));
                ui.add(egui::DragValue::new(&mut self.settings.dup_min_mb).range(0.01..=10_000.0).speed(0.1).suffix(" MB"));
                if self.dup_rx.is_none() {
                    if theme::primary_button(ui, "Find duplicates", pal).clicked() {
                        self.start_dup_scan(ui.ctx());
                    }
                } else if theme::ghost_button(ui, "Cancel").clicked() {
                    self.dup_prog.cancel.store(true, Ordering::Relaxed);
                }
            });
            if self.dup_rx.is_some() {
                ui.add_space(8.0);
                let st = self.dup_prog.stage.load(Ordering::Relaxed);
                let done = self.dup_prog.done.load(Ordering::Relaxed);
                let total = self.dup_prog.total.load(Ordering::Relaxed).max(1);
                let label = match st {
                    0 | 1 => "Grouping files by size…".to_string(),
                    2 => format!("Sampling {} candidates…", fmt_count(total)),
                    3 => format!("Verifying contents · {} of {}", human(done), human(total)),
                    _ => "Finishing…".into(),
                };
                ui.label(egui::RichText::new(label).size(12.5).color(MUTED));
                theme::progress_bar(ui, if st >= 2 { Some(done as f32 / total as f32) } else { None }, pal, 6.0);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            if let Some(groups) = self.dups.clone() {
                ui.add_space(10.0);
                if groups.is_empty() {
                    ui.label(egui::RichText::new("No duplicates found 🎉").color(MUTED));
                }
                for (gi, g) in groups.iter().take(200).enumerate() {
                    let open = self.expanded_dup == Some(gi);
                    let name = std::path::Path::new(&g.paths[0]).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
                    let h = ui.ctx().animate_bool_with_time(Id::unique(("dup", gi)), resp.hovered() || open, 0.12);
                    let p = ui.painter();
                    p.rect_filled(r.shrink2(vec2(0.0, 2.0)), CornerRadius::same(8), lerp_color(SURFACE2, SURFACE3, h));
                    p.text(pos2(r.left() + 12.0, r.center().y), Align2::LEFT_CENTER, format!("{}  {}", if open { "▾" } else { "▸" }, name), FontId::proportional(13.0), TEXT);
                    p.text(
                        pos2(r.right() - 12.0, r.center().y),
                        Align2::RIGHT_CENTER,
                        format!("{} copies × {}  ·  {} wasted", g.paths.len(), human(g.size), human(g.wasted())),
                        FontId::proportional(12.0),
                        MUTED,
                    );
                    if resp.clicked() {
                        self.expanded_dup = if open { None } else { Some(gi) };
                    }
                    if open {
                        egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 6, top: 4, bottom: 8 }).show(ui, |ui| {
                            let is_img = Kind::Image == crate::filetype::classify(name.to_ascii_lowercase().as_bytes(), false);
                            if is_img {
                                if let Some((tex, size, fade)) = self.thumbs.get(ui.ctx(), &g.paths[0]) {
                                    let s = 90.0 / size.y.max(1.0);
                                    ui.add(egui::Image::new((tex, size * s)).tint(with_alpha(Color32::WHITE, fade)).corner_radius(8));
                                }
                            }
                            for (pi, path) in g.paths.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    let tag = if pi == 0 { egui::RichText::new("KEEP").size(10.0).color(GREEN) } else { egui::RichText::new("COPY").size(10.0).color(AMBER) };
                                    ui.label(tag);
                                    ui.add(egui::Label::new(egui::RichText::new(shorten_home(path)).size(12.0).color(MUTED)).truncate());
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("🗑").on_hover_text("Move to Trash").clicked() {
                                            self.confirm_trash = Some(vec![path.clone()]);
                                        }
                                        if ui.small_button("📂").on_hover_text("Reveal").clicked() {
                                            Self::reveal(path);
                                        }
                                    });
                                });
                            }
                            ui.add_space(4.0);
                            if theme::ghost_button(ui, &format!("Trash all copies except the first · free {}", human(g.wasted()))).clicked() {
                                self.confirm_trash = Some(g.paths[1..].to_vec());
                            }
                        });
                    }
                }
                if groups.len() > 200 {
                    ui.label(egui::RichText::new(format!("Showing the 200 biggest of {} groups.", groups.len())).size(12.0).color(FAINT));
                }
            }
        });
    }
}

pub fn shorten_home(p: &str) -> String {
    if let Some(h) = dirs::home_dir() {
        let h = h.to_string_lossy();
        if let Some(rest) = p.strip_prefix(h.as_ref()) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}
