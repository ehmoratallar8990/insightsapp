//! The search page: hero search bar, filter chips, virtualised results table and
//! a detail/preview panel.

use super::app::{InsightApp, fmt_count, fmt_datetime, fmt_time};
use super::theme::{self, *};
use crate::engine::IndexState;
use crate::filetype::Kind;
use crate::search::SortKey;
use crate::storage::human;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align2, Color32, CornerRadius, FontId, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use std::sync::atomic::Ordering;

const ROW_H: f32 = 34.0;
const SEARCH_ID: &str = "main-search";

fn one_line(text: &str, font: FontId, color: Color32, width: f32) -> LayoutJob {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat::simple(font, color));
    job.wrap = TextWrapping { max_width: width.max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    job
}

/// One group of the "group by app" tree.
pub struct TreeGroup {
    pub key: String,
    pub title: String,
    pub path: String,
    pub is_app: bool,
    /// The bundle's index id, for its full on-disk size.
    pub app_id: Option<u32>,
    pub bytes: u64,
    pub newest: u32,
    /// Indices into `results`.
    pub items: Vec<usize>,
}

pub struct TreeCache {
    seq: u64,
    epoch: u64,
    order: (SortKey, bool),
    pub groups: Vec<TreeGroup>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TreeRow {
    Group(usize),
    /// (result index, last in its group)
    Item(usize, bool),
}

/// Folder names that never identify an owner by themselves.
const GENERIC: &[&str] = &[
    "", "library", "application support", "containers", "group containers", "caches", "opt", "homebrew", "cellar", "usr", "local", "lib", "lib64",
    "share", "bin", "sbin", "libexec", "documents", "desktop", "downloads", "projects", "developer", "system", "volumes", "program files",
    "program files (x86)", "appdata", "roaming", "programdata", "programs", "applications", "frameworks", "private", "var", "users", "home",
    "snap", "flatpak", "windows", "system32", "contents", "resources", "src", "packages", "c:", "d:", "e:",
];

/// Best-effort "who owns this file": the first meaningful folder below the home
/// folder / system prefixes, e.g. `~/.lmstudio/…` → "lmstudio",
/// `/opt/homebrew/Cellar/llvm/…` → "llvm", `~/Library/Android/sdk/…` → "Android".
pub fn owner_of_path(path: &str, home: &str) -> (String, String) {
    let comps: Vec<&str> = path.split(['/', '\\']).collect();
    let skip = if !home.is_empty() && path.starts_with(home) { home.split(['/', '\\']).count() } else { 0 };
    let sep = std::path::MAIN_SEPARATOR.to_string();
    for i in skip..comps.len().saturating_sub(1) {
        let c = comps[i];
        if GENERIC.contains(&c.to_ascii_lowercase().as_str()) {
            continue;
        }
        let title = c.trim_start_matches('.');
        let title = if title.is_empty() { c } else { title };
        return (comps[..=i].join(&sep), title.to_string());
    }
    // Only generic folders (e.g. /usr/lib/system/…): group by the top of the tree.
    let n = comps.len().saturating_sub(1).min(if skip > 0 { skip + 1 } else { 3 });
    let key = comps[..n].join(&sep);
    let title = if key.is_empty() { "Other".to_string() } else { key.clone() };
    (key, title)
}

pub fn build_tree(idx: &crate::index::Index, results: &[u32]) -> Vec<TreeGroup> {
    use crate::index::NONE;
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
    let mut groups: Vec<TreeGroup> = Vec::new();
    let mut by_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut by_app: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for (ri, &id) in results.iter().enumerate() {
        if id as usize >= idx.len() {
            continue;
        }
        // Outermost enclosing .app bundle (Xcode's nested apps group under Xcode).
        let mut app = None;
        let mut cur = id;
        while cur != NONE {
            if idx.lower_name(cur).ends_with(b".app") {
                app = Some(cur);
            }
            cur = idx.entries[cur as usize].parent;
        }
        let gi = match app {
            Some(a) => *by_app.entry(a).or_insert_with(|| {
                let name = idx.name(a);
                groups.push(TreeGroup {
                    key: format!("app:{}", idx.path_string(a)),
                    title: name.strip_suffix(".app").unwrap_or(name).to_string(),
                    path: idx.path_string(a),
                    is_app: true,
                    app_id: Some(a),
                    bytes: 0,
                    newest: 0,
                    items: Vec::new(),
                });
                groups.len() - 1
            }),
            None => {
                let (key, title) = owner_of_path(&idx.path_string(id), &home);
                *by_key.entry(key.clone()).or_insert_with(|| {
                    groups.push(TreeGroup { path: key.clone(), key, title, is_app: false, app_id: None, bytes: 0, newest: 0, items: Vec::new() });
                    groups.len() - 1
                })
            }
        };
        let e = &idx.entries[id as usize];
        let g = &mut groups[gi];
        if app == Some(id) {
            // The bundle itself is the group header; don't repeat it as a child.
            g.newest = g.newest.max(e.mtime);
            continue;
        }
        if !e.is_dir() {
            g.bytes += e.size;
        }
        g.newest = g.newest.max(e.mtime);
        g.items.push(ri);
    }
    groups
}

pub const TREE_MAX: usize = 150_000;

/// Name with matched substrings highlighted.
fn highlighted(name: &str, terms: &[String], font: FontId, base: Color32, hi: Color32, width: f32) -> LayoutJob {
    let lower = name.to_ascii_lowercase();
    let mut marks = vec![false; name.len()];
    for t in terms {
        let t = t.to_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        let mut start = 0;
        while let Some(p) = lower[start..].find(&t) {
            let a = start + p;
            for m in marks.iter_mut().skip(a).take(t.len()) {
                *m = true;
            }
            start = a + t.len().max(1);
            if start >= lower.len() {
                break;
            }
        }
    }
    let mut job = LayoutJob::default();
    let mut i = 0;
    while i < name.len() {
        let on = marks[i];
        let mut j = i;
        while j < name.len() && marks[j] == on {
            j += 1;
        }
        // Keep char boundaries valid.
        while j < name.len() && !name.is_char_boundary(j) {
            j += 1;
        }
        let fmt = if on {
            TextFormat { font_id: font.clone(), color: hi, background: with_alpha(hi, 0.16), ..Default::default() }
        } else {
            TextFormat::simple(font.clone(), base)
        };
        job.append(&name[i..j], 0.0, fmt);
        i = j;
    }
    job.wrap = TextWrapping { max_width: width.max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    job
}

impl InsightApp {
    pub fn search_page(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let now = ui.input(|i| i.time);

        // ---------- Search bar ----------
        let bar_h = 54.0;
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), bar_h), Sense::hover());
        let edit_id = Id::unique(SEARCH_ID);
        let focused = ui.memory(|m| m.has_focus(edit_id));
        let f = ui.ctx().animate_bool_with_time(Id::unique("search-focus"), focused, 0.25);
        let p = ui.painter();
        if f > 0.0 {
            theme::glow(p, bar.translate(vec2(0.0, 6.0)), 16, with_alpha(pal.accent, 0.22 * f), 26.0);
        }
        p.rect(bar, CornerRadius::same(16), lerp_color(SURFACE, SURFACE2, f), Stroke::new(1.0 + f * 0.5, lerp_color(BORDER, pal.accent, f)), StrokeKind::Inside);
        p.text(pos2(bar.left() + 22.0, bar.center().y), Align2::CENTER_CENTER, "🔍", FontId::proportional(17.0), lerp_color(MUTED, pal.accent2, f));

        // Right side: results info + help toggle.
        let n_live = self.engine.index.read().live_count();
        let info = if self.results.is_empty() && n_live == 0 {
            String::new()
        } else {
            format!("{} results · {:.1} ms", fmt_count(self.results.len() as u64), self.results_elapsed_us as f64 / 1000.0)
        };
        let help_rect = Rect::from_center_size(pos2(bar.right() - 26.0, bar.center().y), vec2(28.0, 28.0));
        let help = ui.interact(help_rect, Id::unique("help-btn"), Sense::click());
        let hh = ui.ctx().animate_bool_with_time(Id::unique("help-h"), help.hovered() || self.show_help, 0.15);
        ui.painter().circle(help_rect.center(), 12.0, lerp_color(SURFACE3, with_alpha(pal.accent, 0.35), hh), Stroke::new(1.0, BORDER), );
        ui.painter().text(help_rect.center(), Align2::CENTER_CENTER, "?", FontId::proportional(13.0), TEXT);
        if help.clicked() {
            self.show_help = !self.show_help;
        }
        let info_galley = ui.painter().layout_no_wrap(info, FontId::proportional(12.5), MUTED);
        let info_w = info_galley.size().x;
        ui.painter().galley(pos2(help_rect.left() - 12.0 - info_w, bar.center().y - info_galley.size().y / 2.0), info_galley, MUTED);

        let edit_rect = Rect::from_min_max(pos2(bar.left() + 44.0, bar.top() + 8.0), pos2(help_rect.left() - 24.0 - info_w, bar.bottom() - 8.0));
        let prev_query = self.query.clone();
        let edit = egui::TextEdit::singleline(&mut self.query)
            .id(edit_id)
            .frame(egui::Frame::NONE)
            .font(FontId::proportional(18.0))
            .hint_text(egui::RichText::new("Search every file on this computer…   try  ext:pdf  size:>100mb  type:image  label:dog").color(FAINT).size(16.0))
            .align(egui::Align2::LEFT_CENTER)
            .desired_width(edit_rect.width());
        let resp = ui.put(edit_rect, edit);
        if self.focus_search {
            resp.request_focus();
            self.focus_search = false;
        }
        if self.query != prev_query {
            self.selected = Some(0);
        }

        if self.show_help {
            ui.add_space(8.0);
            self.help_card(ui);
        }

        // ---------- Chips ----------
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let all = [None, Some(Kind::Folder), Some(Kind::Image), Some(Kind::Video), Some(Kind::Audio), Some(Kind::Document), Some(Kind::Archive), Some(Kind::Code), Some(Kind::App)];
            for k in all {
                let (label, col) = match k {
                    None => ("✨ All".to_string(), pal.accent),
                    Some(k) => (format!("{} {}", k.icon(), k.label()), kind_color(k)),
                };
                if theme::chip(ui, &label, self.kind == k, col, Id::unique(("chip", k.map(|k| k as u8)))).clicked() {
                    self.kind = k;
                    // Apps read best as a tree grouped by owning app.
                    self.group_by_app = k == Some(Kind::App);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = "ℹ Details";
                if theme::chip(ui, label, self.show_details, pal.accent2, Id::unique("chip-details")).clicked() {
                    self.show_details = !self.show_details;
                }
                if theme::chip(ui, "🌳 Group by app", self.group_by_app, pal.accent, Id::unique("chip-tree"))
                    .on_hover_text("Group results under the app or package that owns them")
                    .clicked()
                {
                    self.group_by_app = !self.group_by_app;
                }
            });
        });
        ui.add_space(12.0);

        // ---------- Body ----------
        let body = ui.available_rect_before_wrap();
        let detail_w = if self.show_details && body.width() > 900.0 { 320.0 } else { 0.0 };
        let table_rect = Rect::from_min_max(body.min, pos2(body.right() - if detail_w > 0.0 { detail_w + 14.0 } else { 0.0 }, body.bottom()));
        let state = self.engine.state.lock().clone();
        let scanning = self.engine.scanning.load(Ordering::Relaxed);

        let mut table_ui = ui.new_child(UiBuilder::new().max_rect(table_rect).id_salt("table"));
        if n_live == 0 && (state != IndexState::Ready || scanning) {
            self.indexing_hero(&mut table_ui);
        } else {
            self.results_table(&mut table_ui, now);
        }
        if detail_w > 0.0 {
            let dr = Rect::from_min_max(pos2(body.right() - detail_w, body.top()), body.max);
            let mut dui = ui.new_child(UiBuilder::new().max_rect(dr).id_salt("details"));
            self.detail_panel(&mut dui);
        }
        ui.allocate_rect(body, Sense::hover());

        self.keyboard(ui, resp.has_focus());
    }

    fn help_card(&mut self, ui: &mut Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let rows: &[(&str, &str)] = &[
                ("foo bar", "name contains foo AND bar"),
                ("\"foo bar\"", "exact phrase"),
                ("!foo   -foo", "exclude"),
                ("jpg|png", "either"),
                ("*.rs   IMG_????.jpg", "wildcards on whole name"),
                ("src/main   path:projects", "match the full path"),
                ("ext:pdf,docx", "by extension"),
                ("type:image|video", "folder image video audio doc archive code app"),
                ("file:   folder:", "only files / only folders"),
                ("size:>100mb   size:1mb..1gb", "by size"),
                ("dm:today   dm:week   dm:>2024-01", "modified date"),
                ("label:dog   label:screenshot", "AI media labels (Media tab)"),
                ("re:^IMG_\\d+", "regular expression"),
                ("hidden:no", "skip dot-files"),
            ];
            egui::Grid::new("help-grid").num_columns(2).spacing(vec2(28.0, 6.0)).show(ui, |ui| {
                for (a, b) in rows {
                    ui.label(egui::RichText::new(*a).monospace().color(self.pal.accent2));
                    ui.label(egui::RichText::new(*b).color(MUTED));
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.label(egui::RichText::new("↑/↓ select · Enter open · ⌘/Ctrl+Enter reveal · ⌘/Ctrl+C copy path · Esc clear").size(12.0).color(FAINT));
        });
    }

    fn indexing_hero(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let rect = ui.max_rect();
        let t = ui.input(|i| i.time) as f32;
        let c = rect.center() - vec2(0.0, 60.0);
        let p = ui.painter().clone();
        // Orbiting dots.
        for i in 0..12 {
            let a = t * 1.6 + i as f32 * std::f32::consts::TAU / 12.0;
            let r = 38.0 + 6.0 * (t * 2.0 + i as f32).sin();
            let pos = c + vec2(a.cos(), a.sin()) * r;
            let col = lerp_color(pal.accent, pal.accent2, i as f32 / 12.0);
            p.circle_filled(pos, 3.0 + 1.5 * ((t * 3.0 + i as f32 * 0.5).sin() * 0.5 + 0.5), col);
        }
        theme::glow(&p, Rect::from_center_size(c, vec2(60.0, 60.0)), 30, with_alpha(pal.accent, 0.35), 40.0);
        theme::logo_mark(&p, c, 16.0, Color32::WHITE);

        let n = self.engine.progress.files.load(Ordering::Relaxed);
        let shown = theme::animated_number(ui, Id::unique("scan-count"), n as f64);
        p.text(c + vec2(0.0, 80.0), Align2::CENTER_CENTER, "Indexing your computer", FontId::proportional(22.0), TEXT);
        p.text(
            c + vec2(0.0, 112.0),
            Align2::CENTER_CENTER,
            format!("{} files and folders found", fmt_count(shown as u64)),
            FontId::proportional(15.0),
            pal.accent2,
        );
        p.text(
            c + vec2(0.0, 140.0),
            Align2::CENTER_CENTER,
            "The first scan takes a moment. After that, Insight opens instantly and stays in sync live.",
            FontId::proportional(13.0),
            MUTED,
        );
        let bar = Rect::from_center_size(c + vec2(0.0, 176.0), vec2(320.0, 6.0));
        let mut bui = ui.new_child(UiBuilder::new().max_rect(bar));
        theme::progress_bar(&mut bui, None, pal, 6.0);
        #[cfg(target_os = "macos")]
        p.text(
            c + vec2(0.0, 214.0),
            Align2::CENTER_CENTER,
            "Tip: grant Full Disk Access in System Settings → Privacy & Security to include protected folders.",
            FontId::proportional(12.0),
            FAINT,
        );
        ui.ctx().request_repaint();
    }

    fn header_cell(&mut self, ui: &Ui, rect: Rect, label: &str, key: SortKey, align_right: bool) {
        let resp = ui.interact(rect, Id::unique(("hdr", label)), Sense::click());
        let active = self.sort == key;
        let h = ui.ctx().animate_bool_with_time(Id::unique(("hdr-h", label)), resp.hovered(), 0.12);
        let col = if active { self.pal.accent2 } else { lerp_color(FAINT, TEXT, h) };
        let arrow = if active { if self.desc { " ↑" } else { " ↓" } } else { "" };
        let text = format!("{}{}", label.to_uppercase(), arrow);
        let (pos, al) = if align_right { (pos2(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER) } else { (pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER) };
        ui.painter().text(pos, al, text, FontId::proportional(11.0), col);
        if resp.clicked() {
            if active {
                self.desc = !self.desc;
            } else {
                self.sort = key;
                self.desc = false;
            }
        }
    }

    fn results_table(&mut self, ui: &mut Ui, now: f64) {
        let pal = self.pal;
        let full = ui.max_rect();
        ui.painter().rect(full, CornerRadius::same(14), SURFACE, Stroke::new(1.0, BORDER), StrokeKind::Inside);
        let w = full.width();
        let cols = [0.40f32, 0.36, 0.10, 0.14];
        let x0 = full.left() + 8.0;
        let mut xs = [x0; 5];
        for i in 0..4 {
            xs[i + 1] = xs[i] + (w - 16.0) * cols[i];
        }
        // Header
        let hdr = Rect::from_min_max(full.min, pos2(full.right(), full.top() + 34.0));
        ui.painter().line_segment([pos2(full.left() + 1.0, hdr.bottom()), pos2(full.right() - 1.0, hdr.bottom())], Stroke::new(1.0, BORDER));
        let hcell = |i: usize| Rect::from_min_max(pos2(xs[i], hdr.top()), pos2(xs[i + 1], hdr.bottom()));
        self.header_cell(ui, hcell(0).with_min_x(hcell(0).left() + 34.0), if self.sort == SortKey::Relevance { "Name · best match" } else { "Name" }, SortKey::Name, false);
        self.header_cell(ui, hcell(1), "Folder", SortKey::Path, false);
        self.header_cell(ui, hcell(2), "Size", SortKey::Size, true);
        self.header_cell(ui, hcell(3), "Modified", SortKey::Modified, true);

        let list_rect = Rect::from_min_max(pos2(full.left(), hdr.bottom() + 2.0), pos2(full.right(), full.bottom() - 4.0));
        let mut lui = ui.new_child(UiBuilder::new().max_rect(list_rect).id_salt("rows"));

        if self.results.is_empty() {
            let c = list_rect.center() - vec2(0.0, 30.0);
            lui.painter().text(c, Align2::CENTER_CENTER, "🔭", FontId::proportional(34.0), MUTED);
            lui.painter().text(c + vec2(0.0, 40.0), Align2::CENTER_CENTER, "No matches", FontId::proportional(17.0), TEXT);
            lui.painter().text(c + vec2(0.0, 64.0), Align2::CENTER_CENTER, "Try fewer words, a wildcard like *.pdf, or another filter.", FontId::proportional(13.0), MUTED);
            return;
        }

        let idx = self.engine.index.clone();
        let idx = idx.read();
        let tree_on = self.group_by_app && self.results.len() <= TREE_MAX;
        if tree_on {
            let epoch = self.results_id_epoch;
            let order = (self.sort, self.desc);
            if self.tree.as_ref().is_none_or(|t| t.seq != self.results_seq || t.epoch != epoch || t.order != order) {
                let mut groups = build_tree(&idx, &self.results);
                // A bundle's own size (incl. resources) when the storage rollup is available.
                if let Some(r) = self.report.as_ref().filter(|r| r.epoch == epoch) {
                    for g in groups.iter_mut() {
                        if let Some(sz) = g.app_id.and_then(|a| r.dir_sizes.get(a as usize)) {
                            g.bytes = g.bytes.max(*sz);
                        }
                    }
                }
                match self.sort {
                    SortKey::Name => groups.sort_by_key(|g| g.title.to_lowercase()),
                    SortKey::Path => groups.sort_by(|a, b| a.path.cmp(&b.path)),
                    SortKey::Modified => groups.sort_by_key(|g| std::cmp::Reverse(g.newest)),
                    _ => groups.sort_by_key(|g| std::cmp::Reverse(g.bytes)),
                }
                if self.desc && self.sort != SortKey::Relevance {
                    groups.reverse();
                }
                // New query: open the top group unless the user already opened some.
                if !groups.iter().any(|g| self.tree_open.contains(&g.key)) {
                    if let Some(g) = groups.first() {
                        self.tree_open.insert(g.key.clone());
                    }
                }
                self.tree = Some(TreeCache { seq: self.results_seq, epoch, order, groups });
            }
            let t = self.tree.as_ref().unwrap();
            self.tree_rows.clear();
            for (gi, g) in t.groups.iter().enumerate() {
                self.tree_rows.push(TreeRow::Group(gi));
                if self.tree_open.contains(&g.key) {
                    let n = g.items.len();
                    self.tree_rows.extend(g.items.iter().enumerate().map(|(k, &ri)| TreeRow::Item(ri, k + 1 == n)));
                }
            }
        } else {
            self.tree_rows.clear();
        }
        let n = if tree_on { self.tree_rows.len() } else { self.results.len() };
        let spacing = 0.0;
        lui.spacing_mut().item_spacing.y = spacing;

        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("results-scroll");
        if self.scroll_to_selected {
            let sel_row = if tree_on { self.selected.and_then(|s| self.tree_rows.iter().position(|r| *r == TreeRow::Item(s, false) || *r == TreeRow::Item(s, true))) } else { self.selected };
            if let Some(sel) = sel_row {
                let (first, last, vh) = self.last_viewport;
                if sel <= first {
                    sa = sa.vertical_scroll_offset(sel as f32 * ROW_H);
                } else if sel + 1 >= last {
                    sa = sa.vertical_scroll_offset(((sel + 1) as f32 * ROW_H - vh + 4.0).max(0.0));
                }
            }
            self.scroll_to_selected = false;
        }
        let terms = self.highlight.clone();
        let mut clicked: Option<usize> = None;
        let mut dbl: Option<usize> = None;
        let mut ctx_action: Option<(usize, &'static str)> = None;
        let mut toggle_group: Option<String> = None;
        let rows = std::mem::take(&mut self.tree_rows);
        let tree = self.tree.take();
        let out = sa.show_rows(&mut lui, ROW_H, n, |ui, range| {
            let first = range.start;
            for row in range.clone() {
                let (i, indent, last) = if tree_on {
                    match rows[row] {
                        TreeRow::Group(gi) => {
                            let g = &tree.as_ref().unwrap().groups[gi];
                            let open = self.tree_open.contains(&g.key);
                            if self.group_row(ui, g, open, &xs, now, row - first) {
                                toggle_group = Some(g.key.clone());
                            }
                            continue;
                        }
                        TreeRow::Item(ri, last) => (ri, 24.0, last),
                    }
                } else {
                    (row, 0.0, false)
                };
                let id = self.results[i];
                if id as usize >= idx.len() {
                    ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
                    continue;
                }
                let e = idx.entries[id as usize];
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                // Staggered entrance animation for fresh results.
                let mut painter = ui.painter().clone();
                let age = now - self.results_at;
                if age < 0.6 && row - first < 30 {
                    let a = theme::ease_out_cubic(((age - (row - first) as f64 * 0.012) / 0.22).clamp(0.0, 1.0) as f32);
                    painter.multiply_opacity(a);
                    ui.ctx().request_repaint();
                }
                let selected = self.selected == Some(i);
                let hov = ui.ctx().animate_bool_with_time(Id::unique(("row-h", id)), resp.hovered(), 0.10);
                let sel = ui.ctx().animate_bool_with_time(Id::unique(("row-s", i)), selected, 0.14);
                let row = rect.shrink2(vec2(6.0, 1.0));
                if sel > 0.0 {
                    painter.rect_filled(row, CornerRadius::same(8), with_alpha(pal.accent, 0.20 * sel));
                    let bar = Rect::from_min_size(pos2(row.left(), row.top() + 7.0), vec2(3.0, (row.height() - 14.0) * sel));
                    painter.rect_filled(bar, CornerRadius::same(2), pal.accent);
                } else if hov > 0.0 {
                    painter.rect_filled(row, CornerRadius::same(8), with_alpha(Color32::WHITE, 0.035 * hov));
                } else if i % 2 == 1 && indent == 0.0 {
                    painter.rect_filled(row, CornerRadius::same(8), with_alpha(Color32::WHITE, 0.012));
                }
                if indent > 0.0 {
                    // Tree guide: vertical line + elbow.
                    let gx = xs[0] + 13.0;
                    let guide = Stroke::new(1.0, with_alpha(self.pal.accent, 0.35));
                    let bottom = if last { rect.center().y } else { rect.bottom() };
                    painter.line_segment([pos2(gx, rect.top()), pos2(gx, bottom)], guide);
                    painter.line_segment([pos2(gx, rect.center().y), pos2(gx + 10.0, rect.center().y)], guide);
                }
                let k = e.kind();
                let ic = Rect::from_center_size(pos2(xs[0] + 14.0 + indent, rect.center().y), vec2(24.0, 24.0));
                painter.rect_filled(ic, CornerRadius::same(7), with_alpha(kind_color(k), 0.16));
                painter.text(ic.center(), Align2::CENTER_CENTER, k.icon(), FontId::proportional(12.5), kind_color(k));
                let name = idx.name(id);
                let name_job = highlighted(name, &terms, FontId::proportional(13.5), TEXT, pal.accent2, xs[1] - xs[0] - 44.0 - indent);
                let g = ui.painter().layout_job(name_job);
                painter.galley(pos2(xs[0] + 34.0 + indent, rect.center().y - g.size().y / 2.0), g, TEXT);
                let parent = if e.parent == crate::index::NONE { String::new() } else { idx.path_string(e.parent) };
                let g = ui.painter().layout_job(one_line(&parent, FontId::proportional(12.5), MUTED, xs[2] - xs[1] - 14.0));
                painter.galley(pos2(xs[1] + 8.0, rect.center().y - g.size().y / 2.0), g, MUTED);
                let size = if e.is_dir() { "—".to_string() } else { human(e.size) };
                painter.text(pos2(xs[3] - 8.0, rect.center().y), Align2::RIGHT_CENTER, size, FontId::proportional(12.5), MUTED);
                painter.text(pos2(xs[4] - 8.0, rect.center().y), Align2::RIGHT_CENTER, fmt_time(e.mtime), FontId::proportional(12.5), MUTED);

                if resp.clicked() {
                    clicked = Some(i);
                }
                if resp.double_clicked() {
                    dbl = Some(i);
                }
                // Drag the file out of the app? egui can't start OS drags yet; offer a context menu instead.
                resp.context_menu(|ui| {
                    clicked = Some(i);
                    if ui.button("↗  Open").clicked() {
                        ctx_action = Some((i, "open"));
                        ui.close();
                    }
                    if ui.button(if cfg!(target_os = "macos") { "📂  Reveal in Finder" } else { "📂  Show in folder" }).clicked() {
                        ctx_action = Some((i, "reveal"));
                        ui.close();
                    }
                    if ui.button("📋  Copy path").clicked() {
                        ctx_action = Some((i, "copy"));
                        ui.close();
                    }
                    if ui.button("📋  Copy name").clicked() {
                        ctx_action = Some((i, "copyname"));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("🗑  Move to Trash").clicked() {
                        ctx_action = Some((i, "trash"));
                        ui.close();
                    }
                });
            }
            (range.start, range.end)
        });
        self.tree_rows = rows;
        self.tree = tree;
        if let Some(k) = toggle_group {
            if !self.tree_open.remove(&k) {
                self.tree_open.insert(k);
            }
        }
        let (first, last) = out.inner;
        self.last_viewport = (first, last, out.inner_rect.height());
        drop(idx);

        if let Some(i) = clicked {
            self.selected = Some(i);
        }
        if let Some(i) = dbl {
            if let Some(p) = self.path_of_row(i) {
                Self::open_path(&p);
            }
        }
        if let Some((i, act)) = ctx_action {
            if let Some(p) = self.path_of_row(i) {
                match act {
                    "open" => Self::open_path(&p),
                    "reveal" => Self::reveal(&p),
                    "copy" => {
                        ui.ctx().copy_text(p);
                        self.toast(ui, "Path copied");
                    }
                    "copyname" => {
                        let name = std::path::Path::new(&p).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        ui.ctx().copy_text(name);
                        self.toast(ui, "Name copied");
                    }
                    "trash" => self.confirm_trash = Some(vec![p]),
                    _ => {}
                }
            }
        }
    }

    /// Paint a tree group header; returns true when clicked (toggle).
    fn group_row(&self, ui: &mut Ui, g: &TreeGroup, open: bool, xs: &[f32; 5], now: f64, nth: usize) -> bool {
        let pal = self.pal;
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
        let mut painter = ui.painter().clone();
        let age = now - self.results_at;
        if age < 0.6 && nth < 30 {
            painter.multiply_opacity(theme::ease_out_cubic(((age - nth as f64 * 0.012) / 0.22).clamp(0.0, 1.0) as f32));
            ui.ctx().request_repaint();
        }
        let hov = ui.ctx().animate_bool_with_time(Id::unique(("grp-h", &g.key)), resp.hovered(), 0.10);
        let row = rect.shrink2(vec2(6.0, 1.0));
        painter.rect_filled(row, CornerRadius::same(8), with_alpha(Color32::WHITE, 0.025 + 0.035 * hov));
        // Chevron rotates open.
        let t = ui.ctx().animate_bool_with_time(Id::unique(("grp-o", &g.key)), open, 0.18);
        let c = pos2(xs[0] + 6.0, rect.center().y);
        let rot = egui::emath::Rot2::from_angle(t * std::f32::consts::FRAC_PI_2);
        let pts: Vec<egui::Pos2> = [vec2(-2.5, -4.0), vec2(2.5, 0.0), vec2(-2.5, 4.0)].iter().map(|v| c + rot * *v).collect();
        painter.add(egui::Shape::line(pts, Stroke::new(1.6, lerp_color(MUTED, TEXT, hov))));
        let ic = Rect::from_center_size(pos2(xs[0] + 26.0, rect.center().y), vec2(24.0, 24.0));
        let col = kind_color(Kind::App);
        if g.is_app {
            theme::gradient_fill(ui.ctx(), &painter, ic, CornerRadius::same(7), col, lerp_color(col, pal.accent2, 0.6));
            painter.text(ic.center(), Align2::CENTER_CENTER, "🚀", FontId::proportional(12.0), Color32::WHITE);
        } else {
            painter.rect_filled(ic, CornerRadius::same(7), with_alpha(pal.accent, 0.20));
            painter.text(ic.center(), Align2::CENTER_CENTER, "📦", FontId::proportional(12.0), pal.accent2);
        }
        let title = format!("{}  ", g.title);
        let tg = ui.painter().layout_no_wrap(title, FontId::proportional(13.5), TEXT);
        let tw = tg.size().x.min(xs[1] - xs[0] - 110.0);
        painter.galley(pos2(xs[0] + 46.0, rect.center().y - tg.size().y / 2.0), tg, TEXT);
        let count = format!("{} item{}", fmt_count(g.items.len() as u64), if g.items.len() == 1 { "" } else { "s" });
        let cg = ui.painter().layout_no_wrap(count, FontId::proportional(11.0), FAINT);
        let pill = Rect::from_min_size(pos2(xs[0] + 46.0 + tw, rect.center().y - 9.0), cg.size() + vec2(12.0, 6.0));
        painter.rect_filled(pill, CornerRadius::same(9), SURFACE3);
        painter.galley(pill.min + vec2(6.0, 3.0), cg, FAINT);
        let sub = super::storage_view::shorten_home(&g.path);
        let gj = ui.painter().layout_job(one_line(&sub, FontId::proportional(12.5), FAINT, xs[2] - xs[1] - 14.0));
        painter.galley(pos2(xs[1] + 8.0, rect.center().y - gj.size().y / 2.0), gj, FAINT);
        painter.text(pos2(xs[3] - 8.0, rect.center().y), Align2::RIGHT_CENTER, human(g.bytes), FontId::proportional(12.5), TEXT);
        painter.text(pos2(xs[4] - 8.0, rect.center().y), Align2::RIGHT_CENTER, fmt_time(g.newest), FontId::proportional(12.5), MUTED);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    }

    pub fn path_of_row(&self, i: usize) -> Option<String> {
        let id = *self.results.get(i)?;
        let idx = self.engine.index.read();
        if (id as usize) < idx.len() { Some(idx.path_string(id)) } else { None }
    }

    fn keyboard(&mut self, ui: &mut Ui, search_focused: bool) {
        if self.confirm_trash.is_some() {
            return;
        }
        let n = self.results.len();
        let (down, up, pgdn, pgup, enter, cmd, esc, copy) = ui.input(|i| {
            (
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::PageDown),
                i.key_pressed(Key::PageUp),
                i.key_pressed(Key::Enter),
                i.modifiers.command,
                i.key_pressed(Key::Escape),
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            )
        });
        if n > 0 {
            let page = (self.last_viewport.1.saturating_sub(self.last_viewport.0)).max(2) - 1;
            let cur = self.selected.unwrap_or(0);
            // Tree mode: move through visible (expanded) items only.
            let visible: Vec<usize> = self.tree_rows.iter().filter_map(|r| if let TreeRow::Item(i, _) = r { Some(*i) } else { None }).collect();
            let tree_nav = self.group_by_app && !self.tree_rows.is_empty();
            let next = if tree_nav {
                if visible.is_empty() {
                    None
                } else {
                    let pos = visible.iter().position(|&v| v == cur);
                    let step = |d: isize| -> usize {
                        let p = pos.map(|p| p as isize).unwrap_or(-1) + d;
                        visible[p.clamp(0, visible.len() as isize - 1) as usize]
                    };
                    if down {
                        Some(step(1))
                    } else if up {
                        Some(step(-1))
                    } else if pgdn {
                        Some(step(page as isize))
                    } else if pgup {
                        Some(step(-(page as isize)))
                    } else {
                        None
                    }
                }
            } else if down {
                Some((cur + 1).min(n - 1))
            } else if up {
                Some(cur.saturating_sub(1))
            } else if pgdn {
                Some((cur + page).min(n - 1))
            } else if pgup {
                Some(cur.saturating_sub(page))
            } else {
                None
            };
            if let Some(nx) = next {
                self.selected = Some(nx);
                self.scroll_to_selected = true;
            }
            if enter {
                if let Some(p) = self.selected.and_then(|s| self.path_of_row(s)) {
                    if cmd { Self::reveal(&p) } else { Self::open_path(&p) }
                }
            }
            // Copy path when nothing is selected in the text field.
            if copy && !(search_focused && !self.query.is_empty() && self.query_selection_nonempty(ui)) {
                if let Some(p) = self.selected.and_then(|s| self.path_of_row(s)) {
                    ui.ctx().copy_text(p);
                    self.toast(ui, "Path copied");
                }
            }
        }
        if esc {
            if self.show_help {
                self.show_help = false;
            } else {
                self.query.clear();
                self.kind = None;
            }
            self.focus_search = true;
        }
        // Type-to-search from anywhere on this page.
        if !search_focused {
            let typed = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if !t.trim().is_empty())));
            if typed && !cmd {
                self.focus_search = true;
            }
        }
    }

    fn query_selection_nonempty(&self, ui: &Ui) -> bool {
        egui::TextEdit::load_state(ui.ctx(), Id::unique(SEARCH_ID))
            .and_then(|s| s.cursor.char_range())
            .is_some_and(|r| r.primary != r.secondary)
    }

    fn detail_panel(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let rect = ui.max_rect();
        ui.painter().rect(rect, CornerRadius::same(14), SURFACE, Stroke::new(1.0, BORDER), StrokeKind::Inside);
        let Some(sel) = self.selected else {
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, "Select a result", FontId::proportional(13.0), FAINT);
            return;
        };
        let Some(&id) = self.results.get(sel) else { return };
        let (e, name, path) = {
            let idx = self.engine.index.read();
            if id as usize >= idx.len() {
                return;
            }
            (idx.entries[id as usize], idx.name(id).to_string(), idx.path_string(id))
        };
        let inner = rect.shrink(16.0);
        let mut ui = ui.new_child(UiBuilder::new().max_rect(inner).id_salt(("detail", id)));
        let k = e.kind();
        let preview_h = 200.0;
        let (pr, _) = ui.allocate_exact_size(vec2(inner.width(), preview_h), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(pr, CornerRadius::same(12), SURFACE2);
        let mut showed_image = false;
        if k == Kind::Image && crate::filetype::is_decodable_image(&name.to_ascii_lowercase()) {
            if let Some((tex, size, fade)) = self.thumbs.get(ui.ctx(), &path) {
                let scale = (pr.width() / size.x).min(pr.height() / size.y);
                let r = Rect::from_center_size(pr.center(), size * scale * (0.96 + 0.04 * fade));
                p.add(
                    egui::epaint::RectShape::filled(r, 10, with_alpha(Color32::WHITE, fade))
                        .with_texture(tex, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
                );
                showed_image = true;
            } else if !self.thumbs.failed(&path) {
                let t = ui.input(|i| i.time) as f32;
                p.text(pr.center(), Align2::CENTER_CENTER, "◌", FontId::proportional(28.0 + (t * 4.0).sin() * 2.0), FAINT);
                ui.ctx().request_repaint();
                showed_image = true;
            }
        }
        if !showed_image {
            theme::glow(&p, Rect::from_center_size(pr.center(), vec2(70.0, 70.0)), 35, with_alpha(kind_color(k), 0.25), 40.0);
            p.text(pr.center(), Align2::CENTER_CENTER, k.icon(), FontId::proportional(54.0), kind_color(k));
        }
        ui.add_space(12.0);
        ui.add(egui::Label::new(egui::RichText::new(&name).size(16.0).strong()).wrap());
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let ext = std::path::Path::new(&name).extension().map(|s| s.to_string_lossy().to_uppercase()).unwrap_or_default();
            let kind_label = if e.is_dir() { "Folder".to_string() } else if ext.is_empty() { k.label().trim_end_matches('s').to_string() } else { format!("{ext} · {}", k.label().trim_end_matches('s')) };
            ui.label(egui::RichText::new(kind_label).size(12.0).color(kind_color(k)));
        });
        ui.add_space(10.0);
        let row = |ui: &mut Ui, a: &str, b: String| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(a).size(12.0).color(FAINT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(b).size(12.5).color(TEXT));
                });
            });
        };
        if e.is_dir() {
            if let Some(r) = self.report.as_ref().filter(|r| r.epoch == self.engine.id_epoch.load(Ordering::Relaxed)) {
                if let Some(s) = r.dir_sizes.get(id as usize) {
                    row(&mut ui, "Total size", human(*s));
                }
                row(&mut ui, "Items", fmt_count(r.children_of(id).len() as u64));
            } else {
                row(&mut ui, "Total size", "open Storage to compute".into());
            }
        } else {
            row(&mut ui, "Size", format!("{} ({} bytes)", human(e.size), fmt_count(e.size)));
            if e.alloc != e.size && e.alloc > 0 {
                row(&mut ui, "On disk", human(e.alloc));
            }
        }
        row(&mut ui, "Modified", fmt_time(e.mtime));
        ui.label(egui::RichText::new(fmt_datetime(e.mtime)).size(11.0).color(FAINT));
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Location").size(12.0).color(FAINT));
        ui.add(egui::Label::new(egui::RichText::new(&path).size(12.0).color(MUTED)).wrap());

        // AI labels.
        let labels = self.media.store.read().labels_for(&path);
        if !labels.is_empty() {
            ui.add_space(10.0);
            ui.label(egui::RichText::new("✨ Labels").size(12.0).color(FAINT));
            ui.horizontal_wrapped(|ui| {
                for (l, p) in labels.iter().filter(|(_, p)| *p >= 0.05) {
                    let txt = format!("{l}  {:.0}%", p * 100.0);
                    if theme::chip(ui, &txt, false, pal.accent2, Id::unique(("lbl", *l))).clicked() {
                        self.query = format!("label:\"{l}\"");
                    }
                }
            });
        }

        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            if theme::primary_button(ui, "Open", pal).clicked() {
                Self::open_path(&path);
            }
            if theme::ghost_button(ui, if cfg!(target_os = "macos") { "Reveal" } else { "Show" }).clicked() {
                Self::reveal(&path);
            }
            if theme::ghost_button(ui, "Copy path").clicked() {
                ui.ctx().copy_text(path.clone());
                self.toast(ui, "Path copied");
            }
            if theme::ghost_button(ui, "🗑").on_hover_text("Move to Trash").clicked() {
                self.confirm_trash = Some(vec![path.clone()]);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::owner_of_path;

    #[test]
    fn owners() {
        let h = "/Users/me";
        assert_eq!(owner_of_path("/Users/me/.lmstudio/extensions/x/libtorch.dylib", h).1, "lmstudio");
        assert_eq!(owner_of_path("/Users/me/miniconda3/lib/python3.12/site-packages/torch/lib/a.dylib", h).1, "miniconda3");
        assert_eq!(owner_of_path("/Users/me/Library/Android/sdk/emulator/lib64/qt/lib/x.dylib", h).1, "Android");
        assert_eq!(owner_of_path("/Users/me/Library/Application Support/JetBrains/DataGrip/plugins/x.exe", h).1, "JetBrains");
        assert_eq!(owner_of_path("/opt/homebrew/Cellar/llvm/20.1.7/lib/libLLVM.dylib", h).1, "llvm");
        assert_eq!(owner_of_path("/Library/Developer/CommandLineTools/usr/lib/x.dylib", h).1, "CommandLineTools");
        // Only generic folders: grouped by the top of the tree.
        assert_eq!(owner_of_path("/usr/lib/system/libfoo.dylib", h).1, "/usr/lib");
    }
}
