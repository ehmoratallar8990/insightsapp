//! Settings: indexed locations, exclusions, behaviour and appearance.

use super::app::{InsightApp, fmt_count};
use super::theme::{self, *};
use crate::index::{default_excludes, default_roots, index_file};
use crate::storage::human;
use egui::{Align2, Color32, CornerRadius, FontId, Id, Sense, Stroke, StrokeKind, Ui, vec2};
use std::sync::atomic::Ordering;

impl InsightApp {
    pub fn settings_page(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("settings-scroll").show(ui, |ui| {
            ui.set_max_width(860.0);
            ui.label(egui::RichText::new("Settings").size(26.0).strong());
            ui.add_space(14.0);

            // ---- Locations ----
            let mut changed = false;
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "Indexed locations", pal);
                let mut remove = None;
                for (i, r) in self.settings.roots.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("💽").size(14.0));
                        ui.label(egui::RichText::new(r).size(13.5));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.settings.roots.len() > 1 && ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    self.settings.roots.remove(i);
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_root).hint_text("Add a folder or drive, e.g. /Volumes/Backup").desired_width(420.0));
                    if theme::ghost_button(ui, "Add").clicked() && std::path::Path::new(self.new_root.trim()).is_dir() {
                        self.settings.roots.push(self.new_root.trim().to_string());
                        self.new_root.clear();
                        changed = true;
                    }
                });
                ui.add_space(12.0);
                theme::section_title(ui, "Excluded paths", pal);
                let mut remove = None;
                for (i, r) in self.settings.excludes.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("⊘").color(FAINT));
                        ui.label(egui::RichText::new(r).size(13.0).color(MUTED));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    self.settings.excludes.remove(i);
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_exclude).hint_text("Exclude a path").desired_width(420.0));
                    if theme::ghost_button(ui, "Exclude").clicked() && !self.new_exclude.trim().is_empty() {
                        self.settings.excludes.push(self.new_exclude.trim().to_string());
                        self.new_exclude.clear();
                        changed = true;
                    }
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if theme::ghost_button(ui, "Restore defaults").clicked() {
                        self.settings.roots = default_roots();
                        self.settings.excludes = default_excludes();
                        changed = true;
                    }
                });
            });
            if changed {
                self.engine.set_roots(self.settings.roots.clone(), self.settings.excludes.clone());
                self.toast(ui, "Locations updated — re-indexing in the background");
            }
            ui.add_space(14.0);

            // ---- Index ----
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "Index", pal);
                let (n, total, mem) = {
                    let idx = self.engine.index.read();
                    let mem = idx.entries.capacity() * std::mem::size_of::<crate::index::Entry>() + idx.names.capacity() * 2;
                    (idx.live_count(), idx.len(), mem)
                };
                let file_size = std::fs::metadata(index_file()).map(|m| m.len()).unwrap_or(0);
                egui::Grid::new("idx-stats").num_columns(2).spacing(vec2(30.0, 6.0)).show(ui, |ui| {
                    let row = |ui: &mut Ui, a: &str, b: String| {
                        ui.label(egui::RichText::new(a).color(MUTED));
                        ui.label(b);
                        ui.end_row();
                    };
                    row(ui, "Objects", fmt_count(n as u64));
                    row(ui, "Tombstones", fmt_count((total - n) as u64));
                    row(ui, "Memory", human(mem as u64));
                    row(ui, "Snapshot on disk", human(file_size));
                    row(ui, "Last full scan", format!("{:.1} s", self.engine.last_scan_ms.load(Ordering::Relaxed) as f64 / 1000.0));
                    row(ui, "Snapshot file", index_file().to_string_lossy().into_owned());
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if self.engine.scanning.load(Ordering::Relaxed) {
                        if theme::ghost_button(ui, "Cancel scan").clicked() {
                            self.engine.cancel_scan();
                        }
                    } else if theme::primary_button(ui, "Rescan now", pal).clicked() {
                        self.engine.rescan();
                        self.toast(ui, "Rescanning in the background");
                    }
                });
                ui.add_space(6.0);
                toggle_row(ui, "Rescan on launch", "Catch changes made while Insight was closed. The saved index is shown instantly meanwhile.", &mut self.settings.rescan_on_launch, pal);
                toggle_row(ui, "Show hidden files", "Include dot-files and dot-folders in results.", &mut self.settings.show_hidden, pal);
                #[cfg(target_os = "macos")]
                {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("🔒 macOS hides some folders (Mail, Messages, Safari…) unless Insight has Full Disk Access.").size(12.5).color(MUTED));
                        if ui.link("Open Privacy settings").clicked() {
                            let _ = open::that_detached("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles");
                        }
                    });
                }
            });
            ui.add_space(14.0);

            // ---- Appearance ----
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "Accent", pal);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    for (i, (name, a, b)) in ACCENTS.iter().enumerate() {
                        let (r, resp) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::click());
                        let sel = ui.ctx().animate_bool_with_time(Id::unique(("acc", i)), self.settings.accent == i, 0.2);
                        let h = ui.ctx().animate_bool_with_time(Id::unique(("acc-h", i)), resp.hovered(), 0.12);
                        let sw = egui::Rect::from_center_size(r.center() - vec2(0.0, 8.0), vec2(36.0, 36.0) * (1.0 + 0.08 * h));
                        if sel > 0.0 {
                            theme::glow(ui.painter(), sw, 18, with_alpha(*a, 0.5 * sel), 16.0);
                        }
                        theme::gradient_fill(ui.ctx(), ui.painter(), sw, CornerRadius::same(18), *a, *b);
                        ui.painter().rect_stroke(sw.expand(3.0 * sel), CornerRadius::same(21), Stroke::new(2.0 * sel, TEXT), StrokeKind::Outside);
                        ui.painter().text(r.center_bottom() - vec2(0.0, 6.0), Align2::CENTER_CENTER, *name, FontId::proportional(11.0), lerp_color(MUTED, TEXT, sel));
                        if resp.clicked() {
                            let ctx = ui.ctx().clone();
                            self.set_accent(&ctx, i);
                        }
                    }
                });
            });
            ui.add_space(14.0);

            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "About", pal);
                ui.label(format!("Insight {} — 100% Rust, cross-platform (macOS · Windows · Linux).", env!("CARGO_PKG_VERSION")));
                ui.label(egui::RichText::new("Everything stays on this computer: the file index, thumbnails and the media model all run locally.").color(MUTED));
                ui.label(egui::RichText::new(format!("Data folder: {}", crate::index::data_dir().display())).size(12.0).color(FAINT));
            });
            ui.add_space(24.0);
        });
    }
}

fn toggle_row(ui: &mut Ui, title: &str, sub: &str, value: &mut bool, pal: Palette) {
    ui.horizontal(|ui| {
        let (r, resp) = ui.allocate_exact_size(vec2(42.0, 24.0), Sense::click());
        if resp.clicked() {
            *value = !*value;
        }
        let t = ui.ctx().animate_bool_with_time(resp.id, *value, 0.18);
        let p = ui.painter();
        if t > 0.0 {
            theme::gradient_fill(ui.ctx(), p, r, CornerRadius::same(12), with_alpha(pal.accent, t), with_alpha(pal.accent2, t));
        }
        p.rect(r, CornerRadius::same(12), with_alpha(SURFACE3, 1.0 - t), Stroke::new(1.0, BORDER), StrokeKind::Inside);
        let x = egui::lerp(r.left() + 12.0..=r.right() - 12.0, theme::ease_out_back(t));
        p.circle_filled(egui::pos2(x, r.center().y), 9.0, Color32::WHITE);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).size(13.5));
            ui.label(egui::RichText::new(sub).size(11.5).color(FAINT));
        });
    });
    ui.add_space(4.0);
}
