//! A zoomable treemap of *photo* folders (like Storage's space map, but weighted by
//! the images inside). Used on the Media page and as the Organize source picker.

use super::cleanup_view::Job;
use super::storage_view::{lerp_rect, squarify};
use super::theme::{self, *};
use crate::engine::Engine;
use crate::filetype::Kind;
use crate::index::{Index, NONE};
use crate::storage::human;
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, vec2};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Image totals per folder.
pub struct PhotoTree {
    epoch: u64,
    bytes: Vec<u64>,
    count: Vec<u32>,
    kids: HashMap<u32, Vec<u32>>,
}

impl PhotoTree {
    pub fn build(idx: &Index, epoch: u64) -> PhotoTree {
        let n = idx.len();
        let mut bytes = vec![0u64; n];
        let mut count = vec![0u32; n];
        for i in (0..n).rev() {
            let e = &idx.entries[i];
            if e.is_deleted() {
                continue;
            }
            if !e.is_dir() && e.kind() == Kind::Image {
                bytes[i] = e.size;
                count[i] = 1;
            }
            if e.parent != NONE && count[i] > 0 {
                let p = e.parent as usize;
                bytes[p] += bytes[i];
                count[p] += count[i];
            }
        }
        let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, e) in idx.entries.iter().enumerate() {
            if e.is_dir() && !e.is_deleted() && count[i] > 0 && e.parent != NONE {
                kids.entry(e.parent).or_default().push(i as u32);
            }
        }
        PhotoTree { epoch, bytes, count, kids }
    }
}

#[derive(Default)]
pub struct FolderMap {
    /// Folder being shown.
    cur: Option<u32>,
    synced_from: String,
    anim: Option<(Rect, f64)>,
    tree: Option<Arc<PhotoTree>>,
    job: Job<PhotoTree>,
}

impl FolderMap {
    /// Draw the map. `path` is the selected folder (kept in sync both ways).
    /// Returns true when the user picked a folder ("Use this folder" / double-click).
    pub fn show(&mut self, ui: &mut Ui, engine: &Arc<Engine>, pal: Palette, salt: &str, path: &mut String, height: f32) -> bool {
        let epoch = engine.id_epoch.load(Ordering::Relaxed);
        if let Some(t) = self.job.poll() {
            self.tree = Some(Arc::new(t));
        }
        if self.tree.as_ref().is_none_or(|t| t.epoch != epoch) && !self.job.running() && engine.index.read().live_count() > 0 {
            let eng = engine.clone();
            self.job.start(ui.ctx(), move || PhotoTree::build(&eng.index.read(), epoch));
        }
        let Some(tree) = self.tree.clone() else {
            ui.horizontal(|ui| {
                theme::spinner(ui, 16.0, pal.accent2);
                ui.label(egui::RichText::new("Mapping photo folders…").color(MUTED));
            });
            return false;
        };
        let idx = engine.index.clone();
        let idx = idx.read();
        // Follow the text field when it changes elsewhere.
        if self.synced_from != *path || self.cur.is_none_or(|c| c as usize >= idx.len()) {
            self.cur = idx.lookup(std::path::Path::new(path.trim())).or_else(|| dirs::home_dir().and_then(|h| idx.lookup(&h)));
            self.synced_from = path.clone();
        }
        let Some(cur) = self.cur else { return false };
        let mut picked = false;
        let mut go: Option<u32> = None;

        // Breadcrumb + totals.
        ui.horizontal(|ui| {
            let mut chain = Vec::new();
            let mut c = cur;
            while c != NONE {
                chain.push(c);
                c = idx.entries[c as usize].parent;
            }
            chain.reverse();
            // Long paths: keep the first and the last few crumbs.
            let skip = chain.len().saturating_sub(5);
            for (j, &id) in chain.iter().enumerate() {
                if j > 0 && j < skip {
                    if j == 1 {
                        ui.label(egui::RichText::new("…").color(FAINT));
                        ui.label(egui::RichText::new("›").color(FAINT));
                    }
                    continue;
                }
                let last = j + 1 == chain.len();
                let name = idx.name(id);
                let txt = egui::RichText::new(if name.is_empty() { "/" } else { name }).size(13.0).color(if last { TEXT } else { MUTED });
                if ui.add(egui::Label::new(txt).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    go = Some(id);
                }
                if !last {
                    ui.label(egui::RichText::new("›").color(FAINT));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::primary_button(ui, "Use this folder", pal).clicked() {
                    picked = true;
                }
                ui.label(
                    egui::RichText::new(format!("{} photos · {}", super::app::fmt_count(tree.count[cur as usize] as u64), human(tree.bytes[cur as usize])))
                        .size(12.5)
                        .color(pal.accent2),
                );
            });
        });
        ui.add_space(6.0);

        let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        let now = ui.input(|i| i.time);
        let mut items: Vec<(u32, u64)> = tree.kids.get(&cur).map(|k| k.iter().map(|&c| (c, tree.bytes[c as usize].max(1))).collect()).unwrap_or_default();
        items.sort_by(|a, b| b.1.cmp(&a.1));
        let in_folders: u64 = items.iter().map(|i| tree.bytes[i.0 as usize]).sum();
        let loose = tree.bytes[cur as usize].saturating_sub(in_folders);
        let mut shown: Vec<(u32, u64)> = items.into_iter().take(48).collect();
        if loose > 0 {
            shown.push((NONE, loose)); // photos directly in this folder
        }
        let tiles = squarify(&shown, area.shrink(1.0));
        let (t_anim, from) = match self.anim {
            Some((r, start)) => {
                let t = ease_out_cubic(((now - start) / 0.4) as f32);
                if t >= 1.0 {
                    self.anim = None;
                } else {
                    ui.ctx().request_repaint();
                }
                (t, Some(r))
            }
            None => (1.0, None),
        };
        let map = |r: Rect| match from {
            Some(f) => {
                let a = (r.min - area.min) / area.size();
                let b = (r.max - area.min) / area.size();
                lerp_rect(Rect::from_min_max(f.min + a * f.size(), f.min + b * f.size()), r, t_anim)
            }
            None => r,
        };
        let p = ui.painter().with_clip_rect(area);
        p.rect_filled(area, CornerRadius::same(10), SURFACE2);
        const TONES: [Color32; 6] = [
            Color32::from_rgb(124, 92, 255),
            Color32::from_rgb(56, 140, 255),
            Color32::from_rgb(20, 184, 166),
            Color32::from_rgb(236, 72, 153),
            Color32::from_rgb(245, 158, 11),
            Color32::from_rgb(99, 102, 241),
        ];
        let mut hovered = None;
        for (i, t) in tiles.iter().enumerate() {
            let r = map(t.rect).shrink(1.5);
            if r.width() < 1.0 || r.height() < 1.0 {
                continue;
            }
            let resp = ui.interact(t.rect, Id::unique((salt, "pt", cur, i)), Sense::click());
            let h = ui.ctx().animate_bool_with_time(Id::unique((salt, "pth", t.id)), resp.hovered(), 0.12);
            let base = if t.id == NONE { kind_color(Kind::Image) } else { lerp_color(TONES[i % TONES.len()], pal.accent, 0.15) };
            p.rect_filled(r, CornerRadius::same(6), lerp_color(with_alpha(base, 0.5), with_alpha(base, 0.85), h));
            p.rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, with_alpha(Color32::WHITE, 0.08 + 0.25 * h)), StrokeKind::Inside);
            let (name, cnt) = if t.id == NONE {
                ("Photos in this folder".to_string(), tree.count[cur as usize] - tree.kids.get(&cur).map(|k| k.iter().map(|&c| tree.count[c as usize]).sum::<u32>()).unwrap_or(0))
            } else {
                (idx.name(t.id).to_string(), tree.count[t.id as usize])
            };
            if r.width() > 60.0 && r.height() > 28.0 && t_anim > 0.6 {
                let mut job = egui::text::LayoutJob::single_section(name.clone(), egui::TextFormat::simple(FontId::proportional(12.5), Color32::WHITE));
                job.wrap = egui::text::TextWrapping { max_width: r.width() - 12.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = ui.painter().layout_job(job);
                p.galley(r.min + vec2(6.0, 5.0), g, Color32::WHITE);
                if r.height() > 44.0 {
                    p.text(r.min + vec2(6.0, 22.0), Align2::LEFT_TOP, format!("{} photos · {}", super::app::fmt_count(cnt as u64), human(t.size)), FontId::proportional(11.0), with_alpha(Color32::WHITE, 0.75));
                }
                if t.id != NONE && tree.kids.contains_key(&t.id) && r.height() > 44.0 {
                    p.text(r.right_top() + vec2(-8.0, 6.0), Align2::RIGHT_TOP, "📁", FontId::proportional(11.0), with_alpha(Color32::WHITE, 0.6));
                }
            }
            if resp.hovered() {
                hovered = Some((name, cnt, t.size, t.id));
            }
            if t.id != NONE {
                if resp.double_clicked() {
                    go = Some(t.id);
                    picked = true;
                } else if resp.clicked() {
                    go = Some(t.id);
                    self.anim = Some((t.rect, now));
                }
            }
        }
        if let Some((name, cnt, size, id)) = hovered {
            let hint = if id == NONE { "pick this folder to include them" } else { "click to open · double-click to use" };
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), Id::unique((salt, "pt-tip")), egui::PopupAnchor::Pointer).show(|ui| {
                ui.label(format!("{name}\n{} photos · {}\n{hint}", super::app::fmt_count(cnt as u64), human(size)));
            });
        }
        if tiles.is_empty() {
            ui.painter().text(area.center(), Align2::CENTER_CENTER, "No photos in this folder", FontId::proportional(14.0), FAINT);
        }
        if let Some(id) = go {
            self.cur = Some(id);
            *path = idx.path_string(id);
            self.synced_from = path.clone();
        } else if picked {
            *path = idx.path_string(cur);
            self.synced_from = path.clone();
        }
        picked
    }
}
