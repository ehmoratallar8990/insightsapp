//! Application shell: state, navigation, page transitions, status bar.

use super::theme::{self, *};
use super::thumbs::Thumbs;
use crate::engine::{Engine, IndexState, SearchRequest};
use crate::filetype::Kind;
use crate::index::{default_excludes, default_roots};
use crate::media::{MediaEngine, SharedLabels};
use crate::search::SortKey;
use crate::storage::{DupGroup, DupProgress, StorageReport};
use crossbeam_channel::Receiver;
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Page {
    Search,
    Health,
    Storage,
    Cleanup,
    Media,
    Organize,
    Settings,
}

impl Page {
    pub const ALL: [Page; 7] = [Page::Search, Page::Health, Page::Storage, Page::Cleanup, Page::Media, Page::Organize, Page::Settings];
    fn label(self) -> &'static str {
        match self {
            Page::Search => "Search",
            Page::Health => "Health",
            Page::Cleanup => "Cleanup",
            Page::Organize => "Organize",
            Page::Storage => "Storage",
            Page::Media => "Media",
            Page::Settings => "Settings",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Page::Search => "🔍",
            Page::Health => "💓",
            Page::Cleanup => "✨",
            Page::Organize => "📂",
            Page::Storage => "📊",
            Page::Media => "🖼",
            Page::Settings => "⚙",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub roots: Vec<String>,
    pub excludes: Vec<String>,
    pub rescan_on_launch: bool,
    pub show_hidden: bool,
    pub accent: usize,
    pub media_scope: String,
    pub dup_scope: String,
    pub dup_min_mb: f32,
    /// Check GitHub for new releases and download them in the background.
    pub auto_update: bool,
    /// Swap a downloaded update in when Insight quits.
    pub install_on_quit: bool,
    /// Add fuzzy (letters-in-order, one-typo) matches when exact hits are few.
    pub fuzzy: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        Settings {
            roots: default_roots(),
            excludes: default_excludes(),
            rescan_on_launch: true,
            show_hidden: true,
            accent: 0,
            media_scope: home.clone(),
            dup_scope: home,
            dup_min_mb: 1.0,
            auto_update: true,
            install_on_quit: true,
            fuzzy: true,
        }
    }
}

pub struct InsightApp {
    pub engine: Arc<Engine>,
    pub media: MediaEngine,
    pub thumbs: Thumbs,
    pub settings: Settings,
    pub pal: Palette,
    pub page: Page,
    pub page_changed_at: f64,
    pub started_at: f64,

    // Search
    pub query: String,
    pub kind: Option<Kind>,
    pub sort: SortKey,
    pub desc: bool,
    pub seq: u64,
    pub last_sent: Option<(String, Option<Kind>, SortKey, bool, u64, (bool, bool))>,
    /// The last response included fuzzy matches.
    pub results_fuzzy: bool,
    pub results: Vec<u32>,
    pub results_seq: u64,
    pub fade_seq: u64,
    pub results_elapsed_us: u64,
    pub highlight: Vec<String>,
    pub results_at: f64,
    pub results_id_epoch: u64,
    pub selected: Option<usize>,
    pub scroll_to_selected: bool,
    pub show_details: bool,
    pub show_help: bool,
    pub focus_search: bool,
    pub last_viewport: (usize, usize, f32),

    // Storage
    pub report: Option<Arc<StorageReport>>,
    pub report_rx: Option<Receiver<StorageReport>>,
    pub report_shown_at: f64,
    pub treemap_path: Vec<u32>,
    pub treemap_anim: Option<(Rect, f64, bool)>,
    pub expanded_suggestion: Option<usize>,
    pub dups: Option<Vec<DupGroup>>,
    pub dup_prog: Arc<DupProgress>,
    pub dup_rx: Option<Receiver<Vec<DupGroup>>>,
    pub expanded_dup: Option<usize>,

    // Media
    pub media_query: String,
    pub media_label: Option<String>,
    pub media_grid: Vec<String>,
    pub media_grid_title: String,
    pub media_grid_at: f64,
    pub media_browse: bool,
    pub media_browse_at: f64,
    pub media_map: super::folder_map::FolderMap,
    pub organize: super::organize_view::OrganizeUi,

    // Settings edit buffers
    pub new_root: String,
    pub new_exclude: String,

    // Overlays
    pub confirm_trash: Option<Vec<String>>,
    pub toast: Option<(String, f64)>,

    // Health / cleanup / updates
    pub monitor: crate::health::Monitor,
    pub cleanup: super::cleanup_view::CleanupUi,
    pub mem_job: super::cleanup_view::Job<()>,
    /// Messages from background threads, shown as toasts.
    pub bg_msgs: Arc<parking_lot::Mutex<Vec<String>>>,
    pub updater: crate::update::Updater,

    // Search tree ("group by app")
    pub group_by_app: bool,
    pub tree: Option<super::search_view::TreeCache>,
    pub tree_open: std::collections::HashSet<String>,
    pub tree_rows: Vec<super::search_view::TreeRow>,
}

impl InsightApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        // Demo / scripted mode: index only these folders and never persist settings.
        if let Ok(r) = std::env::var("INSIGHT_ROOTS") {
            settings.roots = r.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            settings.excludes.clear();
            settings.media_scope = settings.roots[0].clone();
            settings.dup_scope = settings.roots[0].clone();
        }
        let pal = Palette::from_index(settings.accent);
        theme::install_fonts(&cc.egui_ctx);
        theme::apply_style(&cc.egui_ctx, pal);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let media = MediaEngine::start(cc.egui_ctx.clone());
        let engine = Engine::start(
            cc.egui_ctx.clone(),
            settings.roots.clone(),
            settings.excludes.clone(),
            settings.rescan_on_launch,
            SharedLabels(media.store.clone()),
        );
        let now = cc.egui_ctx.input(|i| i.time);
        InsightApp {
            engine,
            media,
            thumbs: Thumbs::new(cc.egui_ctx.clone()),
            settings,
            pal,
            page: Page::Search,
            page_changed_at: now,
            started_at: now,
            query: String::new(),
            kind: None,
            sort: SortKey::Relevance,
            desc: false,
            seq: 0,
            last_sent: None,
            results_fuzzy: false,
            results: Vec::new(),
            results_seq: 0,
            fade_seq: 0,
            results_elapsed_us: 0,
            highlight: Vec::new(),
            results_at: 0.0,
            results_id_epoch: 0,
            selected: None,
            scroll_to_selected: false,
            show_details: true,
            show_help: false,
            focus_search: true,
            last_viewport: (0, 0, 0.0),
            report: None,
            report_rx: None,
            report_shown_at: 0.0,
            treemap_path: Vec::new(),
            treemap_anim: None,
            expanded_suggestion: None,
            dups: None,
            dup_prog: Arc::new(DupProgress::default()),
            dup_rx: None,
            expanded_dup: None,
            media_query: String::new(),
            media_label: None,
            media_grid: Vec::new(),
            media_grid_title: String::new(),
            media_grid_at: 0.0,
            media_browse: false,
            media_browse_at: 0.0,
            media_map: Default::default(),
            organize: super::organize_view::OrganizeUi::new(),
            new_root: String::new(),
            new_exclude: String::new(),
            confirm_trash: None,
            toast: None,
            monitor: crate::health::Monitor::start(cc.egui_ctx.clone()),
            cleanup: Default::default(),
            mem_job: Default::default(),
            bg_msgs: Default::default(),
            updater: crate::update::Updater::new(cc.egui_ctx.clone()),
            group_by_app: false,
            tree: None,
            tree_open: Default::default(),
            tree_rows: Vec::new(),
        }
    }

    pub fn set_page(&mut self, ui: &Ui, p: Page) {
        if self.page != p {
            self.page = p;
            self.page_changed_at = ui.input(|i| i.time);
        }
    }

    pub fn set_accent(&mut self, ctx: &egui::Context, i: usize) {
        self.settings.accent = i;
        self.pal = Palette::from_index(i);
        theme::apply_style(ctx, self.pal);
    }

    pub fn toast(&mut self, ui: &Ui, msg: impl Into<String>) {
        self.toast = Some((msg.into(), ui.input(|i| i.time)));
    }

    /// Send a search if any input changed.
    pub fn pump_search(&mut self) {
        let epoch = self.engine.epoch.load(Ordering::Relaxed);
        let key = (self.query.clone(), self.kind, self.sort, self.desc, epoch, (self.settings.show_hidden, self.settings.fuzzy));
        if self.last_sent.as_ref() != Some(&key) {
            self.seq += 1;
            let user_change = self.last_sent.as_ref().is_none_or(|l| (&l.0, l.1, l.2, l.3) != (&key.0, key.1, key.2, key.3));
            if user_change {
                self.fade_seq = self.seq;
            }
            let mut text = self.query.clone();
            if !self.settings.show_hidden && !text.contains("hidden:") {
                text.push_str(" hidden:no");
            }
            if !self.settings.fuzzy && !text.contains("fuzzy:") {
                text.push_str(" fuzzy:no");
            }
            self.engine.search(SearchRequest { seq: self.seq, text, kind: self.kind, sort: self.sort, desc: self.desc });
            self.last_sent = Some(key);
        }
    }

    fn pump_results(&mut self, now: f64) {
        while let Ok(r) = self.engine.search_rx.try_recv() {
            if r.seq < self.results_seq {
                continue;
            }
            let query_changed = r.seq >= self.fade_seq && self.results_seq < self.fade_seq;
            // Keep selection on the same entry if possible.
            let sel_id = self.selected.and_then(|s| self.results.get(s).copied());
            self.results = r.ids;
            self.results_seq = r.seq;
            self.results_elapsed_us = r.elapsed_us;
            self.highlight = r.highlight;
            self.results_fuzzy = r.fuzzy;
            self.results_id_epoch = self.engine.id_epoch.load(Ordering::Relaxed);
            let _ = r.epoch;
            if query_changed {
                self.results_at = now;
            }
            self.selected = match sel_id {
                Some(id) => self.results.iter().position(|&x| x == id).or(if self.results.is_empty() { None } else { Some(0) }),
                None => if self.results.is_empty() { None } else { Some(0) },
            };
        }
    }

    pub fn reveal(path: &str) {
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
        #[cfg(windows)]
        let _ = std::process::Command::new("explorer").arg(format!("/select,{path}")).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let p = std::path::Path::new(path);
            let target = if p.is_dir() { p } else { p.parent().unwrap_or(p) };
            let _ = open::that_detached(target);
        }
    }

    pub fn open_path(path: &str) {
        let _ = open::that_detached(path);
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        ui.add_space(18.0);
        // Logo
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let (r, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            let t = ui.input(|i| i.time) as f32;
            let p = ui.painter();
            theme::glow(p, r, 9, with_alpha(pal.accent, 0.45), 14.0 + (t * 1.5).sin() * 3.0);
            theme::gradient_fill(ui.ctx(), p, r, CornerRadius::same(9), pal.accent, pal.accent2);
            theme::logo_mark(p, r.center(), 9.0, Color32::WHITE);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(egui::RichText::new("Insight").size(18.0).strong());
                ui.label(egui::RichText::new("find anything, instantly").size(10.5).color(MUTED));
            });
        });
        ui.add_space(26.0);

        let item_h = 40.0;
        let top = ui.cursor().top();
        let sel_idx = Page::ALL.iter().position(|p| *p == self.page).unwrap_or(0) as f32;
        let target_y = top + sel_idx * (item_h + 4.0);
        let y = ui.ctx().animate_value_with_time(Id::unique("nav-pill"), target_y, 0.25);
        let left = ui.max_rect().left() + 10.0;
        let right = ui.max_rect().right() - 10.0;
        let pill = Rect::from_min_max(pos2(left, y), pos2(right, y + item_h));
        ui.painter().rect_filled(pill, CornerRadius::same(10), with_alpha(pal.accent, 0.16));
        ui.painter().rect_stroke(pill, CornerRadius::same(10), Stroke::new(1.0, with_alpha(pal.accent, 0.35)), StrokeKind::Inside);
        let bar = Rect::from_min_size(pos2(left, y + 10.0), vec2(3.0, item_h - 20.0));
        theme::gradient_fill(ui.ctx(), ui.painter(), bar, CornerRadius::same(2), pal.accent, pal.accent2);

        for (i, p) in Page::ALL.iter().enumerate() {
            let r = Rect::from_min_max(pos2(left, top + i as f32 * (item_h + 4.0)), pos2(right, top + i as f32 * (item_h + 4.0) + item_h));
            let resp = ui.interact(r, Id::unique(("nav", i)), Sense::click());
            let hov = ui.ctx().animate_bool_with_time(Id::unique(("nav-h", i)), resp.hovered(), 0.12);
            let active = *p == self.page;
            if !active && hov > 0.0 {
                ui.painter().rect_filled(r, CornerRadius::same(10), with_alpha(Color32::WHITE, 0.03 * hov));
            }
            let col = if active { TEXT } else { lerp_color(MUTED, TEXT, hov) };
            let x_shift = 2.0 * hov;
            ui.painter().text(pos2(r.left() + 18.0 + x_shift, r.center().y), Align2::LEFT_CENTER, p.icon(), FontId::proportional(15.0), col);
            ui.painter().text(pos2(r.left() + 44.0 + x_shift, r.center().y), Align2::LEFT_CENTER, p.label(), FontId::proportional(14.5), col);
            let shortcut = format!("{}{}", if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" }, i + 1);
            ui.painter().text(pos2(r.right() - 12.0, r.center().y), Align2::RIGHT_CENTER, shortcut, FontId::proportional(10.5), with_alpha(FAINT, 0.5 + 0.5 * hov));
            if resp.clicked() {
                self.set_page(ui, *p);
                if *p == Page::Search {
                    self.focus_search = true;
                }
            }
        }
        ui.add_space(Page::ALL.len() as f32 * (item_h + 4.0));

        // Update banner.
        self.update_banner(ui, left, right);

        // Bottom: disk usage mini-card.
        let disks = self.report.as_ref().map(|r| r.disks.clone()).unwrap_or_else(|| cached_disks(ui.ctx()));
        if let Some(d) = disks.first() {
            let bottom = ui.max_rect().bottom() - 16.0;
            let r = Rect::from_min_max(pos2(left, bottom - 74.0), pos2(right, bottom));
            let p = ui.painter();
            p.rect(r, CornerRadius::same(12), SURFACE, Stroke::new(1.0, BORDER), StrokeKind::Inside);
            let used = d.total.saturating_sub(d.available);
            let frac = used as f32 / d.total.max(1) as f32;
            p.text(pos2(r.left() + 12.0, r.top() + 16.0), Align2::LEFT_CENTER, "💽  Disk", FontId::proportional(12.5), TEXT);
            p.text(
                pos2(r.right() - 12.0, r.top() + 16.0),
                Align2::RIGHT_CENTER,
                format!("{:.0}%", frac * 100.0),
                FontId::proportional(12.0),
                MUTED,
            );
            let bar = Rect::from_min_size(pos2(r.left() + 12.0, r.top() + 32.0), vec2(r.width() - 24.0, 6.0));
            p.rect_filled(bar, CornerRadius::same(3), SURFACE3);
            let f = ui.ctx().animate_value_with_time(Id::unique("disk-mini"), frac, 1.0);
            let fill = Rect::from_min_size(bar.min, vec2(bar.width() * f, bar.height()));
            let (a, b) = if frac > 0.9 { (AMBER, RED) } else { (pal.accent, pal.accent2) };
            theme::gradient_fill(ui.ctx(), p, fill, CornerRadius::same(3), a, b);
            p.text(
                pos2(r.left() + 12.0, r.top() + 56.0),
                Align2::LEFT_CENTER,
                format!("{} free of {}", crate::storage::human(d.available), crate::storage::human(d.total)),
                FontId::proportional(11.0),
                MUTED,
            );
        }
    }

    fn update_banner(&mut self, ui: &mut Ui, left: f32, right: f32) {
        use crate::update::State;
        let pal = self.pal;
        let st = self.updater.get();
        let (title, sub, frac) = match &st {
            State::Available(r) => (format!("Update {} available", r.version), "Click to download".to_string(), None),
            State::Downloading(r, f) => (format!("Downloading {}", r.version), format!("{:.0}%", f * 100.0), Some(*f)),
            State::Ready(r, _) => (format!("Insight {} is ready", r.version), "Click to restart & update".to_string(), Some(1.0)),
            State::Installing => ("Installing update…".to_string(), String::new(), None),
            _ => return,
        };
        let bottom = ui.max_rect().bottom() - 16.0 - 74.0 - 10.0;
        let r = Rect::from_min_max(pos2(left, bottom - 62.0), pos2(right, bottom));
        let resp = ui.interact(r, Id::unique("update-banner"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let hov = ui.ctx().animate_bool_with_time(Id::unique("upd-h"), resp.hovered(), 0.12);
        let t = ui.input(|i| i.time) as f32;
        let p = ui.painter();
        theme::glow(p, r.translate(vec2(0.0, 4.0)), 12, with_alpha(pal.accent, 0.25 + 0.15 * (t * 2.0).sin().abs()), 16.0);
        theme::gradient_fill(ui.ctx(), p, r, CornerRadius::same(12), lerp_color(SURFACE2, pal.accent, 0.45 + 0.1 * hov), lerp_color(SURFACE2, pal.accent2, 0.35 + 0.1 * hov));
        p.text(pos2(r.left() + 12.0, r.top() + 18.0), Align2::LEFT_CENTER, format!("✨ {title}"), FontId::proportional(12.5), TEXT);
        p.text(pos2(r.left() + 12.0, r.top() + 38.0), Align2::LEFT_CENTER, sub, FontId::proportional(11.0), with_alpha(TEXT, 0.75));
        if let Some(f) = frac {
            let bar = Rect::from_min_size(pos2(r.left() + 12.0, r.bottom() - 10.0), vec2(r.width() - 24.0, 3.0));
            p.rect_filled(bar, CornerRadius::same(2), with_alpha(Color32::WHITE, 0.15));
            p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f, 3.0)), CornerRadius::same(2), Color32::WHITE);
        }
        if matches!(st, State::Downloading(..) | State::Installing) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        if resp.clicked() {
            match st {
                State::Available(_) => self.updater.download(),
                State::Ready(..) => {
                    if let Err(e) = self.updater.apply(true) {
                        self.toast(ui, format!("Update failed: {e:#}"));
                    }
                }
                _ => {}
            }
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let state = self.engine.state.lock().clone();
        let scanning = self.engine.scanning.load(Ordering::Relaxed);
        let t = ui.input(|i| i.time) as f32;
        ui.horizontal_centered(|ui| {
            ui.add_space(14.0);
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            let (col, label) = match (&state, scanning) {
                (IndexState::Loading, _) => (AMBER, "Loading index…".to_string()),
                (_, true) => {
                    let n = self.engine.progress.files.load(Ordering::Relaxed);
                    (pal.accent2, format!("Refreshing index · {} scanned", fmt_count(n)))
                }
                (IndexState::Scanning, false) => (AMBER, "Scanning…".into()),
                (IndexState::Ready, false) => {
                    let live = self.engine.watch_error.lock().is_none();
                    (GREEN, if live { "Live — watching for changes".into() } else { "Ready".into() })
                }
            };
            let pulse = 0.5 + 0.5 * (t * 3.0).sin();
            ui.painter().circle_filled(r.center(), 3.5 + 2.5 * pulse, with_alpha(col, 0.25 * pulse));
            ui.painter().circle_filled(r.center(), 3.5, col);
            if scanning || state != IndexState::Ready {
                ui.ctx().request_repaint();
            }
            ui.label(egui::RichText::new(label).size(12.0).color(MUTED));
            ui.add_space(12.0);
            let count = self.engine.index.read().live_count();
            ui.label(egui::RichText::new(format!("{} objects indexed", fmt_count(count as u64))).size(12.0).color(FAINT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(14.0);
                let ms = self.engine.last_scan_ms.load(Ordering::Relaxed);
                if ms > 0 {
                    ui.label(egui::RichText::new(format!("full scan {:.1} s", ms as f64 / 1000.0)).size(12.0).color(FAINT));
                }
                if let Some(e) = self.engine.watch_error.lock().clone() {
                    ui.label(egui::RichText::new(format!("⚠ {e}")).size(12.0).color(AMBER));
                }
            });
        });
    }

    fn global_shortcuts(&mut self, ui: &mut Ui) {
        let (cmd, keys) = ui.input(|i| {
            let mut k = Vec::new();
            for (n, key) in [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6, egui::Key::Num7].iter().enumerate() {
                if i.key_pressed(*key) {
                    k.push(n);
                }
            }
            let f = i.key_pressed(egui::Key::F) || i.key_pressed(egui::Key::K);
            (i.modifiers.command, (k, f))
        });
        if cmd {
            if let Some(&n) = keys.0.first() {
                self.set_page(ui, Page::ALL[n]);
            }
            if keys.1 {
                self.set_page(ui, Page::Search);
                self.focus_search = true;
            }
        }
    }

    fn overlays(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // Trash confirmation.
        if let Some(paths) = self.confirm_trash.clone() {
            let mut close = false;
            let modal = egui::Modal::new(Id::unique("confirm-trash"))
                .frame(theme::card().inner_margin(egui::Margin::same(22)))
                .backdrop_color(Color32::from_black_alpha(150))
                .show(&ctx, |ui| {
                    ui.set_max_width(460.0);
                    ui.label(egui::RichText::new("Move to Trash?").size(18.0).strong());
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} item{} will be moved to the {}. You can restore {} from there.",
                            paths.len(),
                            if paths.len() == 1 { "" } else { "s" },
                            if cfg!(windows) { "Recycle Bin" } else { "Trash" },
                            if paths.len() == 1 { "it" } else { "them" }
                        ))
                        .color(MUTED),
                    );
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        for p in paths.iter().take(50) {
                            ui.label(egui::RichText::new(p).size(12.0).color(FAINT));
                        }
                        if paths.len() > 50 {
                            ui.label(egui::RichText::new(format!("…and {} more", paths.len() - 50)).size(12.0).color(FAINT));
                        }
                    });
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, "Move to Trash", Palette { accent: RED, accent2: Color32::from_rgb(251, 113, 133) }).clicked() {
                            let mut ok = 0;
                            let mut failed = 0;
                            for p in &paths {
                                match trash::delete(p) {
                                    Ok(_) => {
                                        ok += 1;
                                        self.engine.forget(std::path::Path::new(p));
                                    }
                                    Err(_) => failed += 1,
                                }
                            }
                            if let Some(g) = self.dups.as_mut() {
                                for grp in g.iter_mut() {
                                    grp.paths.retain(|x| !paths.contains(x));
                                }
                                g.retain(|grp| grp.paths.len() > 1);
                            }
                            self.media_grid.retain(|x| !paths.contains(x));
                            let msg = if failed == 0 {
                                format!("Moved {ok} item{} to Trash", if ok == 1 { "" } else { "s" })
                            } else {
                                format!("Moved {ok}, {failed} failed (permission?)")
                            };
                            self.toast(ui, msg);
                            self.report = None;
                            close = true;
                        }
                        if theme::ghost_button(ui, "Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if close || modal.should_close() {
                self.confirm_trash = None;
            }
        }

        // Toast.
        if let Some((msg, at)) = self.toast.clone() {
            let now = ui.input(|i| i.time);
            let age = now - at;
            if age > 3.2 {
                self.toast = None;
            } else {
                let t_in = ease_out_back(((age) / 0.35) as f32);
                let t_out = 1.0 - ((age - 2.8) / 0.4).clamp(0.0, 1.0) as f32;
                let alpha = t_out.min(1.0);
                let screen = ctx.content_rect();
                let galley = ui.painter().layout_no_wrap(msg, FontId::proportional(13.5), TEXT);
                let size = galley.size() + vec2(36.0, 22.0);
                let y = screen.bottom() - 60.0 + (1.0 - t_in) * 30.0;
                let rect = Rect::from_center_size(pos2(screen.center().x + 110.0, y), size);
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::unique("toast")));
                theme::glow(&painter, rect.translate(vec2(0.0, 6.0)), 12, Color32::from_black_alpha((160.0 * alpha) as u8), 20.0);
                painter.rect(rect, CornerRadius::same(12), with_alpha(SURFACE3, alpha), Stroke::new(1.0, with_alpha(self.pal.accent, 0.6 * alpha)), StrokeKind::Inside);
                painter.circle_filled(pos2(rect.left() + 16.0, rect.center().y), 4.0, with_alpha(GREEN, alpha));
                painter.galley(pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0), galley, with_alpha(TEXT, alpha));
                ctx.request_repaint();
            }
        }
    }
}

thread_local! {
    static DISKS: std::cell::RefCell<Option<(f64, Vec<crate::storage::DiskInfo>)>> = const { std::cell::RefCell::new(None) };
}

fn cached_disks(ctx: &egui::Context) -> Vec<crate::storage::DiskInfo> {
    let now = ctx.input(|i| i.time);
    DISKS.with(|d| {
        let mut d = d.borrow_mut();
        if d.as_ref().is_none_or(|(t, _)| now - t > 30.0) {
            *d = Some((now, crate::storage::disks()));
        }
        d.as_ref().unwrap().1.clone()
    })
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_time(mtime: u32) -> String {
    if mtime == 0 {
        return "—".into();
    }
    let now = crate::index::now_secs() as i64;
    let d = now - mtime as i64;
    if d < 0 {
        return "just now".into();
    }
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        86_400..172_800 => "yesterday".into(),
        172_800..604_800 => format!("{} days ago", d / 86_400),
        _ => {
            use chrono::TimeZone;
            chrono::Local
                .timestamp_opt(mtime as i64, 0)
                .single()
                .map(|t| t.format("%b %-d, %Y").to_string())
                .unwrap_or_default()
        }
    }
}

pub fn fmt_datetime(mtime: u32) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(mtime as i64, 0)
        .single()
        .map(|t| t.format("%A, %B %-d, %Y at %H:%M").to_string())
        .unwrap_or_default()
}

impl eframe::App for InsightApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if std::env::var("INSIGHT_ROOTS").is_ok() {
            return;
        }
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.settings.install_on_quit && matches!(self.updater.get(), crate::update::State::Ready(..)) {
            if let Err(e) = self.updater.apply(false) {
                log::warn!("update on quit failed: {e:#}");
            }
        }
        self.engine.save();
        self.media.cancel();
        self.media.store.read().save();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let now = ui.input(|i| i.time);
        self.thumbs.pump(ui.ctx());
        self.pump_results(now);
        self.pump_search();
        self.global_shortcuts(ui);
        self.monitor.set_active(self.page == Page::Health);
        self.updater.tick(self.settings.auto_update && std::env::var("INSIGHT_SHOT").is_err(), now - self.started_at);
        let msgs: Vec<String> = std::mem::take(&mut *self.bg_msgs.lock());
        if let Some(m) = msgs.into_iter().last() {
            self.toast(ui, m);
        }
        let _ = self.mem_job.poll();

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(212.0)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SIDEBAR).stroke(Stroke::new(1.0, BORDER)))
            .show(ui, |ui| self.sidebar(ui));

        egui::Panel::bottom("status")
            .exact_size(30.0)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SIDEBAR).stroke(Stroke::new(1.0, BORDER)))
            .show(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let full = ui.max_rect();
            ui.painter().rect_filled(full, CornerRadius::ZERO, BG);
            // Ambient gradient glow in the top-right corner.
            let t = now as f32;
            let c = pos2(full.right() - 120.0 + (t * 0.3).sin() * 30.0, full.top() - 60.0);
            theme::glow(ui.painter(), Rect::from_center_size(c, vec2(420.0, 260.0)), 120, with_alpha(self.pal.accent, 0.10), 160.0);

            // Page transition: fade + slide up.
            let k = ease_out_cubic(((now - self.page_changed_at) / 0.30) as f32);
            if k < 1.0 {
                ui.ctx().request_repaint();
            }
            let inner = full.shrink2(vec2(26.0, 20.0)).translate(vec2(0.0, (1.0 - k) * 14.0));
            let mut child = ui.new_child(UiBuilder::new().max_rect(inner).id_salt(("page", self.page as u8)));
            child.multiply_opacity(k);
            match self.page {
                Page::Search => self.search_page(&mut child),
                Page::Health => self.health_page(&mut child),
                Page::Cleanup => self.cleanup_page(&mut child),
                Page::Storage => self.storage_page(&mut child),
                Page::Media => self.media_page(&mut child),
                Page::Organize => self.organize_page(&mut child),
                Page::Settings => self.settings_page(&mut child),
            }
        });

        self.overlays(ui);
        self.debug_screenshot(ui);
    }
}

impl InsightApp {
    /// Developer aid: `INSIGHT_SHOT=/path/out.png [INSIGHT_PAGE=storage] [INSIGHT_QUERY=..] [INSIGHT_DELAY=6]`
    /// renders the app, saves a screenshot of its own window and exits.
    fn debug_screenshot(&mut self, ui: &mut Ui) {
        let Ok(out) = std::env::var("INSIGHT_SHOT") else { return };
        let now = ui.input(|i| i.time);
        let delay: f64 = std::env::var("INSIGHT_DELAY").ok().and_then(|v| v.parse().ok()).unwrap_or(5.0);
        let state_id = Id::unique("debug-shot-state");
        let state: u8 = ui.data(|d| d.get_temp(state_id)).unwrap_or(0);
        if state == 0 && self.engine.index.read().live_count() > 0 {
            if let Ok(p) = std::env::var("INSIGHT_PAGE") {
                let page = match p.as_str() {
                    "storage" => Page::Storage,
                    "health" => Page::Health,
                    "cleanup" => Page::Cleanup,
                    "organize" => Page::Organize,
                    "media" => Page::Media,
                    "settings" => Page::Settings,
                    _ => Page::Search,
                };
                self.set_page(ui, page);
            }
            if let Ok(q) = std::env::var("INSIGHT_QUERY") {
                self.query = q;
            }
            if let Ok(k) = std::env::var("INSIGHT_KIND") {
                self.kind = Kind::parse(&k);
                self.group_by_app = self.kind == Some(Kind::App);
            }
            if std::env::var("INSIGHT_SORT").is_ok_and(|v| v == "size") {
                self.sort = SortKey::Size;
                self.desc = false;
            }
            if let Ok(src) = std::env::var("INSIGHT_ORG_SRC") {
                self.organize.src = src;
                if std::env::var("INSIGHT_ORG_ANALYZE").is_ok() {
                    let ctx = ui.ctx().clone();
                    self.organize_analyze(&ctx);
                }
            }
            if std::env::var("INSIGHT_MEDIA_BROWSE").is_ok() {
                self.media_browse = true;
            }
            if let Ok(t) = std::env::var("INSIGHT_CLEANUP_TAB") {
                self.cleanup.tab = t.parse().unwrap_or(0);
            }
            if let Ok(q) = std::env::var("INSIGHT_MEDIA_QUERY") {
                self.media_query = q.clone();
                self.media.search(q);
            }
            if let Ok(sc) = std::env::var("INSIGHT_MEDIA_SCOPE") {
                self.settings.media_scope = sc;
            }
            if std::env::var("INSIGHT_DUPES").is_ok() {
                let ctx = ui.ctx().clone();
                self.start_dup_scan(&ctx);
                self.expanded_dup = Some(0);
            }
            if std::env::var("INSIGHT_MEDIA_ANALYZE").is_ok() {
                let items = crate::media::candidates(&self.engine.index.read(), &self.settings.media_scope);
                self.media.analyze(items);
            }
            ui.data_mut(|d| d.insert_temp(state_id, 1u8));
        }
        if state == 1 && now - self.started_at > delay {
            let ctx = ui.ctx().clone();
            let cb = egui::ScreenshotCallback::new(move |img: Arc<egui::ColorImage>| {
                let [w, h] = img.size;
                let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
                    let _ = buf.save(&out);
                }
                if std::env::var("INSIGHT_KEEP_OPEN").is_err() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ctx.request_repaint();
            });
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(cb));
            ui.data_mut(|d| d.insert_temp(state_id, 2u8));
        }
        ui.ctx().request_repaint();
    }
}
