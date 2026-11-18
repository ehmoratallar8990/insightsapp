//! Cleanup page — Mole's toolbox with a CleanMyMac-style face:
//! Clean (junk) · Optimize (maintenance + login items) · Uninstall (apps +
//! leftovers) · Projects (rebuildable build folders).

use super::app::{InsightApp, fmt_count};
use super::storage_view::shorten_home;
use super::theme::{self, *};
use crate::cleanup::apps::{self, App, Leftover};
use crate::cleanup::clean::{self, Category, CleanReport, Group};
use crate::cleanup::optimize::{self, StartupItem, Task, TaskState};
use crate::cleanup::purge::{self, Artifact};
use crate::storage::human;
use crossbeam_channel::Receiver;
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A one-shot background computation.
pub struct Job<T> {
    rx: Option<Receiver<T>>,
}

impl<T> Default for Job<T> {
    fn default() -> Self {
        Job { rx: None }
    }
}

impl<T: Send + 'static> Job<T> {
    pub fn start(&mut self, ctx: &egui::Context, f: impl FnOnce() -> T + Send + 'static) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
        self.rx = Some(rx);
    }
    pub fn running(&self) -> bool {
        self.rx.is_some()
    }
    pub fn poll(&mut self) -> Option<T> {
        let r = self.rx.as_ref()?.try_recv().ok()?;
        self.rx = None;
        Some(r)
    }
}

pub enum Confirm {
    Clean,
    Uninstall,
    Purge,
    RemoveStartup(StartupItem),
}

#[derive(Default)]
pub struct CleanupUi {
    pub tab: usize,
    // Clean
    pub cats: Option<Vec<Category>>,
    scan_job: Job<Vec<Category>>,
    scan_started: f64,
    clean_job: Job<CleanReport>,
    last_clean: Option<(CleanReport, f64)>,
    expanded: HashSet<&'static str>,
    // Optimize
    tasks: Vec<Task>,
    task_state: Arc<Mutex<HashMap<&'static str, TaskState>>>,
    batch_running: Arc<std::sync::atomic::AtomicBool>,
    startup: Option<Vec<StartupItem>>,
    // Uninstall
    apps: Option<Vec<App>>,
    apps_job: Job<Vec<App>>,
    app_filter: String,
    app_sel: Option<usize>,
    leftovers: Option<Vec<Leftover>>,
    left_job: Job<(usize, Vec<Leftover>)>,
    uninstall_job: Job<Result<(u64, Vec<String>), String>>,
    app_running: bool,
    // Projects
    arts: Option<Vec<Artifact>>,
    arts_job: Job<Vec<Artifact>>,
    purge_job: Job<(u64, Vec<String>)>,

    confirm: Option<Confirm>,
}

impl CleanupUi {
    pub fn start_scan(&mut self, ctx: &egui::Context) {
        self.scan_started = ctx.input(|i| i.time);
        self.scan_job.start(ctx, clean::scan);
    }
    pub fn scanning(&self) -> bool {
        self.scan_job.running()
    }
}

const TABS: [&str; 4] = ["✨  Clean", "⚡  Optimize", "🗑  Uninstall", "📦  Projects"];

impl InsightApp {
    pub fn cleanup_page(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        self.poll_cleanup_jobs(ui);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Cleanup").size(26.0).strong());
                ui.label(egui::RichText::new("Reclaim space and keep things running smoothly — nothing is touched until you confirm.").color(MUTED));
            });
        });
        ui.add_space(12.0);
        let t = theme::segmented(ui, &TABS, self.cleanup.tab, pal, Id::unique("cleanup-tabs"));
        if t != self.cleanup.tab {
            self.cleanup.tab = t;
            self.page_changed_at = ui.input(|i| i.time) - 0.12;
        }
        ui.add_space(14.0);
        match self.cleanup.tab {
            0 => self.clean_tab(ui),
            1 => self.optimize_tab(ui),
            2 => self.uninstall_tab(ui),
            _ => self.projects_tab(ui),
        }
        self.cleanup_confirm(ui);
    }

    fn poll_cleanup_jobs(&mut self, ui: &Ui) {
        let now = ui.input(|i| i.time);
        let c = &mut self.cleanup;
        if let Some(cats) = c.scan_job.poll() {
            c.cats = Some(cats);
        }
        if let Some(r) = c.clean_job.poll() {
            c.last_clean = Some((r, now));
            c.start_scan(ui.ctx());
        }
        if let Some(a) = c.apps_job.poll() {
            c.apps = Some(a);
        }
        if let Some((i, l)) = c.left_job.poll() {
            if c.app_sel == Some(i) {
                c.leftovers = Some(l);
            }
        }
        if let Some(a) = c.arts_job.poll() {
            c.arts = Some(a);
        }
        let mut msg = None;
        if let Some(r) = c.uninstall_job.poll() {
            msg = Some(match r {
                Ok((freed, failed)) if failed.is_empty() => format!("Uninstalled · {} moved to Trash", human(freed)),
                Ok((freed, failed)) => format!("Uninstalled · {} freed, {} item(s) couldn't be moved", human(freed), failed.len()),
                Err(e) => format!("Uninstall failed: {e}"),
            });
            c.apps = None;
            c.app_sel = None;
            c.leftovers = None;
            c.apps_job.start(ui.ctx(), apps::list);
        }
        if let Some((freed, failed)) = c.purge_job.poll() {
            msg = Some(if failed.is_empty() { format!("Deleted build folders · {} freed", human(freed)) } else { format!("{} freed · {} failed", human(freed), failed.len()) });
            c.arts = None;
            let idx = self.engine.index.clone();
            // Let the file watcher catch up before re-reading the index.
            c.arts_job.start(ui.ctx(), move || {
                std::thread::sleep(std::time::Duration::from_millis(900));
                let idx = idx.read();
                let (s, n) = idx.rollup();
                purge::find(&idx, &s, &n)
            });
        }
        if let Some(m) = msg {
            self.toast(ui, m);
        }
    }

    // ───────────────────────────── Clean ─────────────────────────────

    fn clean_tab(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        if self.cleanup.cats.is_none() && !self.cleanup.scanning() {
            self.cleanup.start_scan(ui.ctx());
        }
        let scanning = self.cleanup.scanning();
        let cleaning = self.cleanup.clean_job.running();
        let (total, sel_bytes, sel_n) = self.cleanup.cats.as_ref().map_or((0, 0, 0), |c| {
            (c.iter().map(|c| c.bytes()).sum::<u64>(), c.iter().map(|c| c.selected_bytes()).sum::<u64>(), c.iter().map(|c| c.selected()).sum::<usize>())
        });

        // Hero.
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 120.0), Sense::hover());
        let p = ui.painter().clone();
        theme::gradient_fill(ui.ctx(), &p, rect, CornerRadius::same(18), lerp_color(BG, pal.accent, 0.30), lerp_color(BG, pal.accent2, 0.16));
        p.rect_stroke(rect, CornerRadius::same(18), Stroke::new(1.0, with_alpha(Color32::WHITE, 0.08)), StrokeKind::Inside);
        let c = pos2(rect.left() + 66.0, rect.center().y);
        let t = ui.input(|i| i.time) as f32;
        if scanning || cleaning {
            for i in 0..10 {
                let a = t * 2.0 + i as f32 * std::f32::consts::TAU / 10.0;
                p.circle_filled(c + vec2(a.cos(), a.sin()) * 30.0, 3.0, lerp_color(pal.accent, pal.accent2, i as f32 / 10.0));
            }
            ui.ctx().request_repaint();
        }
        theme::glow(&p, Rect::from_center_size(c, vec2(54.0, 54.0)), 27, with_alpha(pal.accent, 0.30), 26.0);
        p.text(c, Align2::CENTER_CENTER, "✨", FontId::proportional(30.0), Color32::WHITE);
        let shown = theme::animated_number(ui, Id::unique("junk-total"), total as f64) as u64;
        let (headline, sub) = if cleaning {
            ("Cleaning…".to_string(), "Deleting the selected items".to_string())
        } else if scanning && self.cleanup.cats.is_none() {
            ("Scanning for junk…".to_string(), "Caches, logs, trash, browser and developer caches".to_string())
        } else if let Some((r, at)) = self.cleanup.last_clean.as_ref().filter(|(_, at)| ui.input(|i| i.time) - at < 8.0) {
            let _ = at;
            (
                format!("{} freed", human(r.freed)),
                if r.failed.is_empty() { format!("{} items removed", fmt_count(r.removed as u64)) } else { format!("{} removed · {} in use or protected", r.removed, r.failed.len()) },
            )
        } else {
            (format!("{} of junk found", human(shown)), format!("{} selected · {} items", human(sel_bytes), fmt_count(sel_n as u64)))
        };
        p.text(pos2(rect.left() + 124.0, rect.center().y - 14.0), Align2::LEFT_CENTER, headline, FontId::proportional(26.0), TEXT);
        p.text(pos2(rect.left() + 124.0, rect.center().y + 18.0), Align2::LEFT_CENTER, sub, FontId::proportional(13.5), with_alpha(TEXT, 0.7));
        let br = Rect::from_min_max(pos2(rect.right() - 320.0, rect.center().y - 18.0), pos2(rect.right() - 24.0, rect.center().y + 18.0));
        let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(br).layout(egui::Layout::right_to_left(egui::Align::Center)));
        if !scanning && !cleaning {
            if sel_n > 0 && theme::primary_button(&mut bui, &format!("Clean {}", human(sel_bytes)), pal).clicked() {
                self.cleanup.confirm = Some(Confirm::Clean);
            }
            if theme::ghost_button(&mut bui, "⟳ Rescan").clicked() {
                self.cleanup.start_scan(ui.ctx());
            }
        }
        ui.add_space(16.0);

        let Some(cats) = self.cleanup.cats.as_mut() else { return };
        if cats.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("✨ Spotless — nothing to clean.").size(15.0).color(GREEN));
            });
            return;
        }
        let started = self.page_changed_at;
        let mut reveal: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("clean-scroll").show(ui, |ui| {
            let mut n = 0;
            for g in Group::ALL {
                let in_group: Vec<usize> = cats.iter().enumerate().filter(|(_, c)| c.group == g).map(|(i, _)| i).collect();
                if in_group.is_empty() {
                    continue;
                }
                let gb: u64 = in_group.iter().map(|&i| cats[i].bytes()).sum();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(g.label().to_uppercase()).size(11.5).strong().color(FAINT));
                    ui.label(egui::RichText::new(human(gb)).size(11.5).color(FAINT));
                });
                ui.add_space(4.0);
                for i in in_group {
                    n += 1;
                    let a = theme::appear(ui, started, 0.03 * n as f64);
                    let cat = &mut cats[i];
                    let open = self.cleanup.expanded.contains(cat.id);
                    let mut toggle_open = false;
                    egui::Frame::new()
                        .fill(SURFACE)
                        .stroke(Stroke::new(1.0, BORDER))
                        .corner_radius(CornerRadius::same(14))
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .multiply_with_opacity(a)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let mut all = cat.items.iter().all(|i| i.on);
                                if theme::check(ui, &mut all, pal.accent, Id::unique(("cat", cat.id))) {
                                    for it in cat.items.iter_mut() {
                                        it.on = all;
                                    }
                                }
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new(cat.icon).size(18.0));
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(cat.title).size(14.5).strong().color(TEXT));
                                        if !cat.safe {
                                            ui.label(egui::RichText::new("REVIEW").size(10.0).strong().color(AMBER));
                                        }
                                    });
                                    ui.label(egui::RichText::new(cat.detail).size(12.0).color(MUTED));
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let chev = ui.ctx().animate_bool_with_time(Id::unique(("chev", cat.id)), open, 0.18);
                                    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                                    let ang = chev * std::f32::consts::FRAC_PI_2;
                                    let c = r.center();
                                    let pts = [vec2(-3.0, -5.0), vec2(3.0, 0.0), vec2(-3.0, 5.0)].map(|v| c + egui::emath::Rot2::from_angle(ang) * v);
                                    ui.painter().add(egui::Shape::line(pts.to_vec(), Stroke::new(1.6, if resp.hovered() { TEXT } else { MUTED })));
                                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        toggle_open = true;
                                    }
                                    ui.label(egui::RichText::new(human(cat.bytes())).size(14.5).strong().color(TEXT));
                                    ui.label(egui::RichText::new(format!("{} items", fmt_count(cat.items.len() as u64))).size(12.0).color(FAINT));
                                });
                            });
                            if open {
                                ui.add_space(6.0);
                                ui.separator();
                                let max = cat.items.first().map(|i| i.bytes).unwrap_or(1).max(1);
                                for (k, it) in cat.items.iter_mut().enumerate().take(200) {
                                    ui.horizontal(|ui| {
                                        ui.add_space(26.0);
                                        theme::check(ui, &mut it.on, pal.accent, Id::unique(("item", cat.id, k)));
                                        let name = std::path::Path::new(&it.path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(it.path.clone());
                                        let resp = ui.add(egui::Label::new(egui::RichText::new(name).size(13.0).color(TEXT)).sense(Sense::click()).truncate());
                                        if resp.on_hover_text(shorten_home(&it.path)).double_clicked() {
                                            reveal = Some(it.path.clone());
                                        }
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.add_space(30.0);
                                            ui.label(egui::RichText::new(human(it.bytes)).size(12.5).color(MUTED));
                                            let (r, _) = ui.allocate_exact_size(vec2(80.0, 4.0), Sense::hover());
                                            ui.painter().rect_filled(r, CornerRadius::same(2), SURFACE3);
                                            ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(r.width() * it.bytes as f32 / max as f32, r.height())), CornerRadius::same(2), with_alpha(pal.accent2, 0.8));
                                        });
                                    });
                                }
                                if cat.items.len() > 200 {
                                    ui.label(egui::RichText::new(format!("…and {} more", cat.items.len() - 200)).size(12.0).color(FAINT));
                                }
                            }
                        });
                    if toggle_open {
                        if open {
                            self.cleanup.expanded.remove(cat.id);
                        } else {
                            self.cleanup.expanded.insert(cat.id);
                        }
                    }
                    ui.add_space(8.0);
                }
                ui.add_space(8.0);
            }
            ui.label(egui::RichText::new("Double-click an item to reveal it. Cleaned items are deleted permanently — apps rebuild caches as needed. Quit browsers first for the best result.").size(12.0).color(FAINT));
        });
        if let Some(p) = reveal {
            Self::reveal(&p);
        }
    }

    // ───────────────────────────── Optimize ─────────────────────────────

    fn optimize_tab(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        if self.cleanup.tasks.is_empty() {
            self.cleanup.tasks = optimize::tasks();
        }
        if self.cleanup.startup.is_none() {
            self.cleanup.startup = Some(optimize::startup_items());
        }
        let states = self.cleanup.task_state.lock().clone();
        let batch = self.cleanup.batch_running.load(std::sync::atomic::Ordering::Relaxed);
        if batch || states.values().any(|s| *s == TaskState::Running) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut run: Vec<Task> = Vec::new();
        let mut remove_item: Option<StartupItem> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("opt-scroll").show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::section_title(ui, "Maintenance", pal);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if batch {
                        theme::spinner(ui, 18.0, pal.accent2);
                    } else if theme::primary_button(ui, "⚡ Run recommended", pal).clicked() {
                        run = self.cleanup.tasks.iter().filter(|t| t.recommended).cloned().collect();
                    }
                });
            });
            let w = ui.available_width();
            let cols = if w > 900.0 { 2 } else { 1 };
            let gap = 12.0;
            let cw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
            let ch = 92.0;
            let n = self.cleanup.tasks.len();
            let rows = n.div_ceil(cols);
            let (grid, _) = ui.allocate_exact_size(vec2(w, rows as f32 * (ch + gap)), Sense::hover());
            for (i, task) in self.cleanup.tasks.iter().enumerate() {
                let a = theme::appear(ui, self.page_changed_at, 0.03 * i as f64);
                let r = Rect::from_min_size(pos2(grid.left() + (i % cols) as f32 * (cw + gap), grid.top() + (i / cols) as f32 * (ch + gap) + (1.0 - a) * 8.0), vec2(cw, ch));
                let mut cu = ui.new_child(egui::UiBuilder::new().max_rect(r).id_salt(("task", task.id)));
                cu.multiply_opacity(a);
                let st = states.get(task.id).cloned().unwrap_or(TaskState::Idle);
                theme::card_at(&mut cu, r, ("task-card", task.id), |ui| {
                    ui.horizontal(|ui| {
                        let (ir, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
                        ui.painter().rect_filled(ir, CornerRadius::same(12), with_alpha(pal.accent, 0.16));
                        ui.painter().text(ir.center(), Align2::CENTER_CENTER, task.icon, FontId::proportional(19.0), pal.accent2);
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.set_max_width(cw - 200.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(task.title).size(14.5).strong().color(TEXT));
                                if task.admin {
                                    ui.label(egui::RichText::new("🔒").size(11.0)).on_hover_text("Asks for your password");
                                }
                            });
                            let (txt, col) = match &st {
                                TaskState::Done(m) => (format!("✔ {m}"), GREEN),
                                TaskState::Failed(m) => (format!("✖ {m}"), RED),
                                _ => (task.detail.to_string(), MUTED),
                            };
                            ui.add(egui::Label::new(egui::RichText::new(txt).size(12.0).color(col)).wrap());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match st {
                            TaskState::Running => theme::spinner(ui, 20.0, pal.accent2),
                            _ => {
                                if theme::ghost_button(ui, "Run").clicked() {
                                    run.push(task.clone());
                                }
                            }
                        });
                    });
                });
            }

            ui.add_space(6.0);
            theme::section_title(ui, "Login & background items", pal);
            ui.label(egui::RichText::new("Programs that start automatically. Removing one stops it launching at login (it moves to the Trash).").size(12.5).color(MUTED));
            ui.add_space(8.0);
            let items = self.cleanup.startup.clone().unwrap_or_default();
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                if items.is_empty() {
                    ui.label(egui::RichText::new("Nothing starts automatically.").color(MUTED));
                }
                for (i, it) in items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(if it.user { "👤" } else { "🛡" }).size(14.0));
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&it.name).size(13.5).color(TEXT));
                            ui.label(egui::RichText::new(&it.detail).size(11.5).color(FAINT));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if it.user {
                                if theme::ghost_button(ui, "Remove").clicked() {
                                    remove_item = Some(it.clone());
                                }
                            } else {
                                ui.label(egui::RichText::new("system").size(11.5).color(FAINT)).on_hover_text("Installed for all users — remove its app to get rid of it");
                            }
                            if theme::ghost_button(ui, "Reveal").clicked() {
                                Self::reveal(&it.path);
                            }
                        });
                    });
                    if i + 1 < items.len() {
                        ui.add_space(2.0);
                        ui.separator();
                    }
                }
            });
        });
        if let Some(it) = remove_item {
            self.cleanup.confirm = Some(Confirm::RemoveStartup(it));
        }
        if !run.is_empty() {
            let states = self.cleanup.task_state.clone();
            let flag = self.cleanup.batch_running.clone();
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            for t in &run {
                states.lock().insert(t.id, TaskState::Running);
            }
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                // Sequential: admin tasks would otherwise stack password prompts.
                for t in run {
                    let r = optimize::run(&t);
                    states.lock().insert(t.id, r);
                    ctx.request_repaint();
                }
                flag.store(false, std::sync::atomic::Ordering::Relaxed);
                ctx.request_repaint();
            });
        }
    }

    // ───────────────────────────── Uninstall ─────────────────────────────

    fn uninstall_tab(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        if self.cleanup.apps.is_none() && !self.cleanup.apps_job.running() {
            self.cleanup.apps_job.start(ui.ctx(), apps::list);
        }
        let Some(list) = self.cleanup.apps.clone() else {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("Measuring installed apps…").size(15.0));
                ui.add_space(8.0);
                theme::progress_bar(ui, None, pal, 6.0);
            });
            return;
        };
        let body = ui.available_rect_before_wrap();
        let lw = (body.width() * 0.42).clamp(300.0, 460.0);
        let left = Rect::from_min_max(body.min, pos2(body.left() + lw, body.bottom()));
        let right = Rect::from_min_max(pos2(left.right() + 14.0, body.top()), body.max);
        ui.allocate_rect(body, Sense::hover());

        // App list.
        let mut select: Option<usize> = None;
        theme::card_at(ui, left, "apps-list", |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("🔍").color(MUTED));
                ui.add(egui::TextEdit::singleline(&mut self.cleanup.app_filter).hint_text(format!("Filter {} apps", list.len())).frame(egui::Frame::NONE).desired_width(f32::INFINITY));
            });
            ui.separator();
            let q = self.cleanup.app_filter.to_lowercase();
            let max = list.first().map(|a| a.bytes).unwrap_or(1).max(1);
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("apps-scroll").show(ui, |ui| {
                for (i, a) in list.iter().enumerate() {
                    if !q.is_empty() && !a.name.to_lowercase().contains(&q) {
                        continue;
                    }
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
                    let sel = self.cleanup.app_sel == Some(i);
                    let hov = ui.ctx().animate_bool_with_time(Id::unique(("app-h", i)), resp.hovered(), 0.1);
                    let s = ui.ctx().animate_bool_with_time(Id::unique(("app-s", i)), sel, 0.15);
                    let p = ui.painter();
                    if s > 0.0 {
                        p.rect_filled(r, CornerRadius::same(10), with_alpha(pal.accent, 0.20 * s));
                    } else if hov > 0.0 {
                        p.rect_filled(r, CornerRadius::same(10), with_alpha(Color32::WHITE, 0.035 * hov));
                    }
                    let ic = Rect::from_center_size(pos2(r.left() + 22.0, r.center().y), vec2(30.0, 30.0));
                    theme::gradient_fill(ui.ctx(), p, ic, CornerRadius::same(9), lerp_color(pal.accent, kind_hash_color(&a.name), 0.6), kind_hash_color(&a.name));
                    let initial: String = a.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                    p.text(ic.center(), Align2::CENTER_CENTER, initial, FontId::proportional(15.0), Color32::WHITE);
                    p.text(pos2(r.left() + 46.0, r.top() + 15.0), Align2::LEFT_CENTER, &a.name, FontId::proportional(13.5), TEXT);
                    let sub = if a.version.is_empty() { a.source.to_string() } else { format!("{} · v{}", a.source, a.version) };
                    p.text(pos2(r.left() + 46.0, r.top() + 32.0), Align2::LEFT_CENTER, sub, FontId::proportional(11.0), FAINT);
                    p.text(pos2(r.right() - 8.0, r.top() + 15.0), Align2::RIGHT_CENTER, human(a.bytes), FontId::proportional(12.5), MUTED);
                    let bar = Rect::from_min_size(pos2(r.right() - 88.0, r.top() + 30.0), vec2(80.0, 3.0));
                    p.rect_filled(bar, CornerRadius::same(2), SURFACE3);
                    p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * a.bytes as f32 / max as f32, 3.0)), CornerRadius::same(2), with_alpha(pal.accent2, 0.8));
                    if resp.clicked() {
                        select = Some(i);
                    }
                }
            });
        });
        if let Some(i) = select {
            if self.cleanup.app_sel != Some(i) {
                self.cleanup.app_sel = Some(i);
                self.cleanup.leftovers = None;
                let app = list[i].clone();
                self.cleanup.app_running = apps::is_running(&app);
                self.cleanup.left_job.start(ui.ctx(), move || (i, apps::leftovers(&app)));
            }
        }

        // Detail.
        theme::card_at(ui, right, "app-detail", |ui| {
            let Some(app) = self.cleanup.app_sel.and_then(|i| list.get(i)).cloned() else {
                let c = ui.max_rect().center();
                ui.painter().text(c - vec2(0.0, 12.0), Align2::CENTER_CENTER, "🗑", FontId::proportional(34.0), FAINT);
                ui.painter().text(c + vec2(0.0, 26.0), Align2::CENTER_CENTER, "Pick an app to see everything it installed", FontId::proportional(14.0), MUTED);
                return;
            };
            ui.label(egui::RichText::new(&app.name).size(22.0).strong());
            let mut meta = vec![app.source.to_string()];
            if !app.version.is_empty() {
                meta.push(format!("v{}", app.version));
            }
            if !app.bundle_id.is_empty() {
                meta.push(app.bundle_id.clone());
            }
            ui.label(egui::RichText::new(meta.join("  ·  ")).size(12.5).color(MUTED));
            if !app.path.is_empty() {
                ui.label(egui::RichText::new(shorten_home(&app.path)).size(12.0).color(FAINT));
            }
            if self.cleanup.app_running {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("⚠ This app is running — quit it before uninstalling.").size(12.5).color(AMBER));
            }
            ui.add_space(10.0);
            ui.label(egui::RichText::new("WHAT WILL BE REMOVED").size(11.5).strong().color(FAINT));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let mut on = true;
                ui.add_enabled_ui(false, |ui| theme::check(ui, &mut on, pal.accent, Id::unique("app-self")));
                ui.label(egui::RichText::new(if app.uninstall_cmd.is_some() { "Run the app's uninstaller" } else { "Application" }).size(13.5).color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(human(app.bytes)).size(12.5).color(MUTED));
                });
            });
            let busy = self.cleanup.uninstall_job.running();
            match self.cleanup.leftovers.as_mut() {
                None => {
                    ui.add_space(6.0);
                    theme::progress_bar(ui, None, pal, 4.0);
                }
                Some(left) => {
                    egui::ScrollArea::vertical().max_height(ui.available_height() - 70.0).id_salt("left-scroll").show(ui, |ui| {
                        for (k, l) in left.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                theme::check(ui, &mut l.on, if l.personal { AMBER } else { pal.accent }, Id::unique(("left", k)));
                                ui.label(egui::RichText::new(l.kind).size(12.0).color(if l.personal { AMBER } else { pal.accent2 }));
                                let name = std::path::Path::new(&l.path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                ui.add(egui::Label::new(egui::RichText::new(name).size(13.0).color(TEXT)).truncate()).on_hover_text(shorten_home(&l.path));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(egui::RichText::new(if l.bytes > 0 { human(l.bytes) } else { "—".into() }).size(12.5).color(MUTED));
                                });
                            });
                        }
                        if left.is_empty() {
                            ui.label(egui::RichText::new("No leftover files found.").size(12.5).color(FAINT));
                        } else if left.iter().any(|l| l.personal) {
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new("Amber items are your settings and data — kept unless you tick them.").size(11.5).color(FAINT));
                        }
                    });
                    let total = app.bytes + left.iter().filter(|l| l.on).map(|l| l.bytes).sum::<u64>();
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if busy {
                                    theme::spinner(ui, 20.0, RED);
                                } else if theme::primary_button(ui, &format!("Uninstall · {}", human(total)), Palette { accent: RED, accent2: Color32::from_rgb(251, 113, 133) }).clicked() {
                                    self.cleanup.confirm = Some(Confirm::Uninstall);
                                }
                            });
                        });
                    });
                }
            }
        });
    }

    // ───────────────────────────── Projects ─────────────────────────────

    fn projects_tab(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let ready = self.engine.index.read().live_count() > 0;
        if self.cleanup.arts.is_none() && !self.cleanup.arts_job.running() && ready {
            let idx = self.engine.index.clone();
            self.cleanup.arts_job.start(ui.ctx(), move || {
                let idx = idx.read();
                let (s, n) = idx.rollup();
                purge::find(&idx, &s, &n)
            });
        }
        let busy = self.cleanup.purge_job.running();
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "Build folders that can be regenerated (npm install, cargo build…). Active in the last {} days = unticked.",
                    purge::RECENT_DAYS
                ))
                .size(12.5)
                .color(MUTED),
            );
        });
        ui.add_space(10.0);
        let Some(arts) = self.cleanup.arts.as_mut() else {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("Looking for project build folders in the index…").size(15.0));
                ui.add_space(8.0);
                theme::progress_bar(ui, None, pal, 6.0);
            });
            return;
        };
        let sel: u64 = arts.iter().filter(|a| a.on).map(|a| a.bytes).sum();
        let total: u64 = arts.iter().map(|a| a.bytes).sum();
        let n_on = arts.iter().filter(|a| a.on).count();
        let mut ask = false;
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let mut all = !arts.is_empty() && arts.iter().all(|a| a.on);
                if theme::check(ui, &mut all, pal.accent, Id::unique("arts-all")) {
                    for a in arts.iter_mut() {
                        a.on = all;
                    }
                }
                ui.label(egui::RichText::new(format!("{} build folders · {}", arts.len(), human(total))).size(15.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if busy {
                        theme::spinner(ui, 20.0, pal.accent2);
                    } else if n_on > 0 && theme::primary_button(ui, &format!("Delete {} · {}", n_on, human(sel)), pal).clicked() {
                        ask = true;
                    }
                });
            });
            ui.add_space(6.0);
            if arts.is_empty() {
                ui.label(egui::RichText::new("No rebuildable project folders found under your home folder.").color(MUTED));
                return;
            }
            let max = arts.first().map(|a| a.bytes).unwrap_or(1).max(1);
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("arts-scroll").show(ui, |ui| {
                for (k, a) in arts.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        theme::check(ui, &mut a.on, pal.accent, Id::unique(("art", k)));
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&a.project).size(13.5).strong().color(TEXT));
                                ui.label(egui::RichText::new(a.kind).size(11.5).color(pal.accent2));
                                ui.label(egui::RichText::new(format!("{} files", fmt_count(a.files as u64))).size(11.5).color(FAINT));
                            });
                            ui.add(egui::Label::new(egui::RichText::new(shorten_home(&a.path)).size(11.5).color(FAINT)).truncate());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(human(a.bytes)).size(13.0).strong().color(TEXT));
                            let (r, _) = ui.allocate_exact_size(vec2(90.0, 4.0), Sense::hover());
                            ui.painter().rect_filled(r, CornerRadius::same(2), SURFACE3);
                            ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(r.width() * a.bytes as f32 / max as f32, 4.0)), CornerRadius::same(2), with_alpha(pal.accent2, 0.8));
                            let days = a.idle_secs / 86_400;
                            let (txt, col) = if days < purge::RECENT_DAYS { ("active".to_string(), AMBER) } else { (format!("idle {days} d"), FAINT) };
                            ui.label(egui::RichText::new(txt).size(11.5).color(col));
                        });
                    });
                    ui.add_space(4.0);
                }
            });
        });
        if ask {
            self.cleanup.confirm = Some(Confirm::Purge);
        }
    }

    // ───────────────────────────── Confirm ─────────────────────────────

    fn cleanup_confirm(&mut self, ui: &mut Ui) {
        let Some(kind) = self.cleanup.confirm.as_ref() else { return };
        let (title, body, button, danger) = match kind {
            Confirm::Clean => {
                let (b, n) = self.cleanup.cats.as_ref().map_or((0, 0), |c| (c.iter().map(|c| c.selected_bytes()).sum::<u64>(), c.iter().map(|c| c.selected()).sum::<usize>()));
                ("Clean selected junk?".to_string(), format!("{} in {} items will be permanently deleted. Apps rebuild their caches automatically.", human(b), fmt_count(n as u64)), format!("Clean {}", human(b)), false)
            }
            Confirm::Uninstall => {
                let name = self.cleanup.app_sel.and_then(|i| self.cleanup.apps.as_ref()?.get(i)).map(|a| a.name.clone()).unwrap_or_default();
                let n = self.cleanup.leftovers.as_ref().map(|l| l.iter().filter(|l| l.on).count()).unwrap_or(0);
                (format!("Uninstall {name}?"), format!("The app and {n} related item(s) will be moved to the {}.", if cfg!(windows) { "Recycle Bin" } else { "Trash" }), "Uninstall".into(), true)
            }
            Confirm::Purge => {
                let (b, n) = self.cleanup.arts.as_ref().map_or((0, 0), |a| (a.iter().filter(|a| a.on).map(|a| a.bytes).sum::<u64>(), a.iter().filter(|a| a.on).count()));
                ("Delete build folders?".to_string(), format!("{n} folders ({}) will be permanently deleted. Rebuild them with npm install / cargo build when you need them.", human(b)), "Delete".into(), true)
            }
            Confirm::RemoveStartup(it) => (format!("Remove {}?", it.name), "It will no longer start automatically. The file is moved to the Trash so you can restore it.".into(), "Remove".into(), true),
        };
        let mut close = false;
        let mut go = false;
        let modal = egui::Modal::new(Id::unique("cleanup-confirm"))
            .frame(theme::card().inner_margin(egui::Margin::same(22)))
            .backdrop_color(Color32::from_black_alpha(150))
            .show(ui.ctx(), |ui| {
                ui.set_max_width(440.0);
                ui.label(egui::RichText::new(&title).size(18.0).strong());
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&body).color(MUTED));
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let pal = if danger { Palette { accent: RED, accent2: Color32::from_rgb(251, 113, 133) } } else { self.pal };
                    if theme::primary_button(ui, &button, pal).clicked() {
                        go = true;
                    }
                    if theme::ghost_button(ui, "Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if go {
            let ctx = ui.ctx().clone();
            match self.cleanup.confirm.take() {
                Some(Confirm::Clean) => {
                    let cats = self.cleanup.cats.clone().unwrap_or_default();
                    self.cleanup.clean_job.start(&ctx, move || clean::clean(&cats));
                }
                Some(Confirm::Uninstall) => {
                    if let (Some(app), Some(left)) = (self.cleanup.app_sel.and_then(|i| self.cleanup.apps.as_ref()?.get(i)).cloned(), self.cleanup.leftovers.clone()) {
                        self.cleanup.uninstall_job.start(&ctx, move || apps::uninstall(&app, &left).map_err(|e| e.to_string()));
                    }
                }
                Some(Confirm::Purge) => {
                    let arts = self.cleanup.arts.clone().unwrap_or_default();
                    self.cleanup.purge_job.start(&ctx, move || purge::purge(&arts));
                }
                Some(Confirm::RemoveStartup(it)) => {
                    match trash::delete(&it.path) {
                        Ok(()) => self.toast(ui, format!("{} removed from login items", it.name)),
                        Err(e) => self.toast(ui, format!("Couldn't remove: {e}")),
                    }
                    self.cleanup.startup = None;
                }
                None => {}
            }
        } else if close || modal.should_close() {
            self.cleanup.confirm = None;
        }
    }
}

/// A stable pleasant colour per name (for app initials).
fn kind_hash_color(s: &str) -> Color32 {
    let h = xxhash_rust::xxh3::xxh3_64(s.as_bytes());
    let palette = [
        Color32::from_rgb(129, 140, 248),
        Color32::from_rgb(56, 189, 248),
        Color32::from_rgb(52, 211, 153),
        Color32::from_rgb(251, 191, 36),
        Color32::from_rgb(244, 114, 182),
        Color32::from_rgb(167, 139, 250),
        Color32::from_rgb(251, 146, 60),
    ];
    palette[(h % palette.len() as u64) as usize]
}
