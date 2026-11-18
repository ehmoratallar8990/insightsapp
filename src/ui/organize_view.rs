//! Organize page: sort camera photos into Year › Event › Camera › Format, with an
//! editable preview (event names, tags, merges, camera names, formats) before
//! anything is written. Non-destructive and fully on device.

use super::app::{InsightApp, fmt_count};
use super::cleanup_view::Job;
use super::folder_map::FolderMap;
use super::storage_view::shorten_home;
use super::theme::{self, *};
use crate::organize::{self, Event, Format, Options, Progress, Report, Scan};
use crate::storage::human;
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub struct OrganizeUi {
    pub src: String,
    pub dest: String,
    pub opts: Options,
    browse: bool,
    map: FolderMap,
    progress: Arc<Progress>,
    scan_job: Job<Scan>,
    scan: Option<Scan>,
    review: HashSet<usize>,
    planned_sensitivity: f32,
    pub events: Vec<Event>,
    scanned: (String, String),
    run_job: Job<Result<Report, String>>,
    report: Option<Report>,
    undo_job: Job<Result<usize, String>>,
    confirm: bool,
    collapsed_years: HashSet<i32>,
    planned_gap: f32,
    shown_at: f64,
}

impl OrganizeUi {
    pub fn new() -> Self {
        let pics = dirs::picture_dir().or_else(|| dirs::home_dir().map(|h| h.join("Pictures"))).unwrap_or_default();
        OrganizeUi {
            src: pics.to_string_lossy().into_owned(),
            dest: pics.join("Organized").to_string_lossy().into_owned(),
            opts: Options::default(),
            browse: false,
            map: FolderMap::default(),
            progress: Arc::new(Progress::default()),
            scan_job: Job::default(),
            scan: None,
            review: HashSet::new(),
            planned_sensitivity: 0.35,
            events: Vec::new(),
            scanned: (String::new(), String::new()),
            run_job: Job::default(),
            report: None,
            undo_job: Job::default(),
            confirm: false,
            collapsed_years: HashSet::new(),
            planned_gap: 6.0,
            shown_at: 0.0,
        }
    }
}

fn date_range(e: &Event) -> String {
    let (a, b) = (e.start, e.end);
    if a.date() == b.date() {
        format!("{} · {}–{}", a.format("%a %b %-d, %Y"), a.format("%H:%M"), b.format("%H:%M"))
    } else if a.format("%Y%m").to_string() == b.format("%Y%m").to_string() {
        format!("{}–{}", a.format("%b %-d"), b.format("%-d, %Y"))
    } else {
        format!("{} – {}", a.format("%b %-d"), b.format("%b %-d, %Y"))
    }
}

/// Free space on the volume holding `path` (or its nearest existing parent).
fn free_space(path: &str) -> Option<u64> {
    let disks = crate::storage::disks();
    let p = Path::new(path);
    disks.iter().filter(|d| p.starts_with(&d.mount)).max_by_key(|d| d.mount.len()).map(|d| d.available)
}

impl InsightApp {
    pub fn organize_analyze(&mut self, ctx: &egui::Context) {
        let o = &mut self.organize;
        let src = PathBuf::from(o.src.trim());
        let dest = PathBuf::from(o.dest.trim());
        o.progress = Arc::new(Progress::default());
        let prog = o.progress.clone();
        o.report = None;
        o.scanned = (o.src.clone(), o.dest.clone());
        let quality = o.opts.rejects;
        o.scan_job.start(ctx, move || organize::scan(&src, Some(&dest), &prog, quality));
    }

    fn organize_replan(&mut self) {
        let Some(mut scan) = self.organize.scan.take() else { return };
        // Content labels from the on-device model (Media tab) → file-name suffixes and
        // event-name suggestions. RAWs borrow their JPEG twin's label.
        {
            let store = self.media.store.read();
            for p in scan.photos.iter_mut() {
                p.label = store.labels_for(&p.path.to_string_lossy()).first().filter(|(_, pr)| *pr >= 0.25).map(|(l, _)| l.to_string());
            }
        }
        organize::share_labels(&mut scan);
        organize::mark_rejects(&mut scan, self.organize.opts.reject_sensitivity);
        let by_path: std::collections::HashMap<PathBuf, String> = scan.photos.iter().filter_map(|p| Some((p.path.clone(), p.label.clone()?))).collect();
        let labels = move |p: &Path| by_path.get(p).cloned();
        let src = PathBuf::from(self.organize.scanned.0.trim());
        self.organize.events = organize::plan(&scan, &self.organize.opts, &src, &labels);
        self.organize.planned_gap = self.organize.opts.gap_hours;
        self.organize.planned_sensitivity = self.organize.opts.reject_sensitivity;
        self.organize.review.clear();
        self.organize.scan = Some(scan);
    }

    pub fn organize_page(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let now = ui.input(|i| i.time);
        if let Some(s) = self.organize.scan_job.poll() {
            self.organize.scan = Some(s);
            self.organize.shown_at = now;
            if std::env::var("INSIGHT_ORG_DAYS").is_ok() {
                self.organize.opts.split_days = true;
            }
            self.organize_replan();
            // Screenshot hooks: INSIGHT_ORG_RENAME="2025-06-14=Ana & Luis Wedding", INSIGHT_ORG_REVIEW=1
            if let Ok(r) = std::env::var("INSIGHT_ORG_RENAME") {
                for pair in r.split(';') {
                    if let Some((d, n)) = pair.split_once('=') {
                        for e in self.organize.events.iter_mut().filter(|e| e.start.format("%Y-%m-%d").to_string() == d) {
                            e.name = n.to_string();
                        }
                    }
                }
            }
            if std::env::var("INSIGHT_ORG_REVIEW").is_ok() {
                if let Some(scan) = &self.organize.scan {
                    for (i, e) in self.organize.events.iter().enumerate() {
                        if e.photos.iter().any(|&k| scan.photos[k].reject) {
                            self.organize.review.insert(i);
                        }
                    }
                }
            }
        }
        if let Some(r) = self.organize.run_job.poll() {
            match r {
                Ok(rep) => {
                    let msg = format!("Organized {} files", fmt_count(rep.created as u64));
                    self.organize.report = Some(rep);
                    self.toast(ui, msg);
                }
                Err(e) => self.toast(ui, format!("Organize failed: {e}")),
            }
        }
        if let Some(r) = self.organize.undo_job.poll() {
            match r {
                Ok(n) => {
                    self.organize.report = None;
                    self.toast(ui, format!("Undone — removed {} created files (originals untouched)", fmt_count(n as u64)));
                }
                Err(e) => self.toast(ui, format!("Undo failed: {e}")),
            }
        }
        let scanning = self.organize.scan_job.running();
        let running = self.organize.run_job.running();
        if scanning || running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(80));
        }

        theme::debug_scroll(egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("org-scroll")).show(ui, |ui| {
            ui.label(egui::RichText::new("Organize").size(26.0).strong());
            ui.label(
                egui::RichText::new("Sort camera photos into Year › Event › Camera › Format. Preview and adjust everything first — originals are never moved or changed.")
                    .color(MUTED),
            );
            ui.add_space(14.0);

            // ---------- Folders ----------
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "Folders", pal);
                egui::Grid::new("org-folders").num_columns(2).spacing(vec2(12.0, 10.0)).show(ui, |ui| {
                    ui.label(egui::RichText::new("Photos from").color(MUTED));
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.organize.src).desired_width(460.0).hint_text("A messy folder, a card dump, ~/Pictures…"));
                        if theme::chip(ui, "📁 Browse", self.organize.browse, pal.accent2, Id::unique("org-browse")).clicked() {
                            self.organize.browse = !self.organize.browse;
                        }
                    });
                    ui.end_row();
                    ui.label(egui::RichText::new("Organize into").color(MUTED));
                    ui.add(egui::TextEdit::singleline(&mut self.organize.dest).desired_width(460.0));
                    ui.end_row();
                });
                if self.organize.browse {
                    ui.add_space(8.0);
                    let eng = self.engine.clone();
                    if self.organize.map.show(ui, &eng, pal, "org-map", &mut self.organize.src, 260.0) {
                        self.organize.browse = false;
                    }
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if scanning {
                        let (d, t) = (self.organize.progress.done.load(Ordering::Relaxed), self.organize.progress.total.load(Ordering::Relaxed));
                        theme::spinner(ui, 18.0, pal.accent2);
                        let what = if self.organize.progress.phase.load(Ordering::Relaxed) == 1 { "Checking focus & exposure" } else { "Reading camera data" };
                        ui.label(egui::RichText::new(format!("{what} · {} / {}", fmt_count(d), fmt_count(t))).color(MUTED));
                        if theme::ghost_button(ui, "Stop").clicked() {
                            self.organize.progress.cancel.store(true, Ordering::Relaxed);
                        }
                    } else {
                        let label = if self.organize.scan.is_some() { "⟳ Analyze again" } else { "Analyze photos" };
                        if theme::primary_button(ui, label, pal).clicked() && !running {
                            let ctx = ui.ctx().clone();
                            self.organize_analyze(&ctx);
                        }
                        ui.label(egui::RichText::new("Reads only the EXIF header of each file — even 80 MB RAWs take milliseconds.").size(12.0).color(FAINT));
                    }
                });
                if scanning {
                    let (d, t) = (self.organize.progress.done.load(Ordering::Relaxed), self.organize.progress.total.load(Ordering::Relaxed));
                    ui.add_space(6.0);
                    theme::progress_bar(ui, (t > 0).then(|| d as f32 / t as f32), pal, 6.0);
                }
            });
            ui.add_space(14.0);

            // ---------- Options ----------
            let mut replan = false;
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "Options", pal);
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Include").color(MUTED));
                    for (i, f) in Format::ALL.iter().enumerate() {
                        let n = self.organize.scan.as_ref().map(|s| s.photos.iter().filter(|p| p.format == *f).count()).unwrap_or(0);
                        let label = if self.organize.scan.is_some() { format!("{} · {}", f.label(), fmt_count(n as u64)) } else { f.label().to_string() };
                        let on = self.organize.opts.formats[i];
                        if theme::chip(ui, &format!("{} {label}", if on { "✔" } else { "  " }), on, pal.accent, Id::unique(("org-fmt", i))).clicked() {
                            self.organize.opts.formats[i] = !on;
                        }
                    }
                });
                ui.add_space(8.0);
                ui.columns(2, |c| {
                    toggle(&mut c[0], "Separate formats", "RAW / JPG / HEIC subfolders inside each camera", &mut self.organize.opts.split_formats, pal);
                    toggle(&mut c[0], "Zip RAW files", "One RAW.zip per camera per event (stored, not recompressed)", &mut self.organize.opts.zip_raw, pal);
                    toggle(&mut c[1], "Symlinks instead of copies", "Links point at the originals — uses no space, breaks if originals move", &mut self.organize.opts.symlink, pal);
                    toggle(&mut c[1], "Also copy non-camera images", "Screenshots, downloads & exports go to “Other images”", &mut self.organize.opts.include_others, pal);
                    toggle(&mut c[0], "Descriptive file names", "IMG_0412.CR3 → IMG_0412_beach.CR3 (content label, else event name)", &mut self.organize.opts.suffix, pal);
                    let before = self.organize.opts.split_days;
                    toggle(&mut c[1], "Split multi-day events into days", "Trip to Europe / Day 1 - 2025-07-14, Day 2 - …", &mut self.organize.opts.split_days, pal);
                    if before != self.organize.opts.split_days {
                        let on = self.organize.opts.split_days;
                        for e in self.organize.events.iter_mut().filter(|e| e.days() > 1) {
                            e.by_day = on;
                        }
                    }
                    toggle(&mut c[0], "Move rejects aside", "Blurry or badly exposed shots → “Rejects” inside their camera folder", &mut self.organize.opts.rejects, pal);
                });
                if self.organize.opts.rejects {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Reject sensitivity").color(MUTED));
                        let r = ui.add(egui::Slider::new(&mut self.organize.opts.reject_sensitivity, 0.0..=1.0).show_value(false));
                        ui.label(egui::RichText::new(match self.organize.opts.reject_sensitivity {
                            x if x < 0.2 => "only hopeless shots",
                            x if x < 0.5 => "clearly blurry",
                            x if x < 0.8 => "strict",
                            _ => "very strict",
                        }).size(12.0).color(FAINT));
                        if (r.drag_stopped() || (r.changed() && !r.dragged())) && (self.organize.opts.reject_sensitivity - self.organize.planned_sensitivity).abs() > 0.001 {
                            if let Some(scan) = self.organize.scan.as_mut() {
                                organize::mark_rejects(scan, self.organize.opts.reject_sensitivity);
                                self.organize.planned_sensitivity = self.organize.opts.reject_sensitivity;
                            }
                        }
                        if self.organize.scan.as_ref().is_some_and(|s| s.photos.iter().all(|p| p.quality.is_none())) {
                            ui.label(egui::RichText::new("· analyze again to check focus").size(12.0).color(AMBER));
                        }
                    });
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("New event after").color(MUTED));
                    let r = ui.add(egui::Slider::new(&mut self.organize.opts.gap_hours, 1.0..=48.0).step_by(0.5).suffix(" h without photos"));
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        replan = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Tags for everything").color(MUTED));
                    ui.add(egui::TextEdit::singleline(&mut self.organize.opts.tags).desired_width(360.0).hint_text("e.g. Portfolio, Client: Ana"));
                    ui.label(egui::RichText::new(if cfg!(target_os = "macos") { "→ Finder tags" } else if cfg!(windows) { "(not supported on Windows)" } else { "→ user.xdg.tags" }).size(11.5).color(FAINT));
                });
            });
            if replan && self.organize.scan.is_some() && (self.organize.opts.gap_hours - self.organize.planned_gap).abs() > 0.01 {
                self.organize_replan();
            }
            ui.add_space(14.0);

            let Some(mut scan) = self.organize.scan.take() else {
                if !scanning {
                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("How it works").size(15.0).strong());
                        ui.add_space(4.0);
                        for l in [
                            "1.  Insight reads each file's camera make, model and capture time (JPEG, HEIC, CR3, CR2, NEF, ARW, RAF, DNG…).",
                            "2.  Photos are grouped into events by time; screenshots and downloads without camera data are left alone.",
                            "3.  You rename events, merge them, add tags, rename cameras and pick formats in the preview.",
                            "4.  Organize copies (or links) into  2025 / 2025-07-14 Wedding / Canon EOS R8 / RAW. Undo removes only what it created.",
                        ] {
                            ui.label(egui::RichText::new(l).color(MUTED));
                        }
                    });
                }
                return;
            };
            self.organize_preview(ui, &mut scan, now);
            self.organize.scan = Some(scan);
        });
        self.organize_confirm(ui);
    }

    fn organize_preview(&mut self, ui: &mut Ui, scan: &mut Scan, now: f64) {
        let pal = self.pal;
        let a = ease_out_cubic(((now - self.organize.shown_at) / 0.35).clamp(0.0, 1.0) as f32);
        if a < 1.0 {
            ui.ctx().request_repaint();
        }
        let o = &self.organize;
        let mut cams: BTreeMap<String, usize> = BTreeMap::new();
        for p in &scan.photos {
            *cams.entry(p.camera.clone()).or_default() += 1;
        }
        let ops = organize::build_ops(scan, &o.events, &o.opts, Path::new(o.dest.trim()));
        let bytes: u64 = ops.iter().map(|op| op.size).sum();
        let other_bytes: u64 = scan.others.iter().map(|x| x.1).sum();

        // ---------- Stats ----------
        let stats = [
            ("📷", fmt_count(scan.photos.len() as u64), "camera photos".to_string(), pal.accent),
            ("📅", fmt_count(o.events.len() as u64), "events".to_string(), pal.accent2),
            ("🎞", fmt_count(cams.len() as u64), if cams.len() == 1 { "camera".into() } else { "cameras".into() }, GREEN),
            ("🖼", fmt_count(scan.others.len() as u64), format!("not from a camera · {} (left alone)", human(other_bytes)), AMBER),
            ("🗑", fmt_count(if o.opts.rejects { scan.photos.iter().filter(|p| p.reject).count() } else { 0 } as u64), "rejects (blurry / exposure)".to_string(), RED),
        ];
        ui.columns(5, |cols| {
            for (i, (icon, v, l, c)) in stats.iter().enumerate() {
                let ui = &mut cols[i];
                ui.multiply_opacity(a);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
                        ui.painter().rect_filled(r, CornerRadius::same(12), with_alpha(*c, 0.16));
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, *icon, FontId::proportional(18.0), *c);
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(v).size(20.0).strong());
                            ui.label(egui::RichText::new(l).size(11.5).color(MUTED));
                        });
                    });
                });
            }
        });
        ui.add_space(14.0);

        // ---------- Cameras ----------
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title(ui, "Cameras", pal);
            ui.label(egui::RichText::new("Folder names are editable — e.g. “Canon EOS R8” → “R8”.").size(12.0).color(FAINT));
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                for (cam, n) in &cams {
                    egui::Frame::new().fill(SURFACE2).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(if cam.to_lowercase().contains("iphone") || cam.to_lowercase().contains("pixel") || cam.to_lowercase().contains("galaxy") { "📱" } else { "📷" }).size(14.0));
                            let name = self.organize.opts.camera_names.entry(cam.clone()).or_insert_with(|| cam.clone());
                            ui.add(egui::TextEdit::singleline(name).desired_width(150.0).frame(egui::Frame::NONE).font(FontId::proportional(13.5)));
                            ui.label(egui::RichText::new(fmt_count(*n as u64)).size(12.0).color(pal.accent2));
                        });
                    });
                }
            });
        });
        ui.add_space(14.0);

        // ---------- Events ----------
        ui.horizontal(|ui| {
            theme::section_title(ui, "Preview", pal);
            ui.label(egui::RichText::new("Rename events, untick to skip, merge with the one above, add tags. Folder names start with the date so they sort chronologically.").size(12.0).color(FAINT));
        });
        ui.add_space(4.0);
        let mut years: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
        for (i, e) in self.organize.events.iter().enumerate() {
            years.entry(organize::year_of(e)).or_default().push(i);
        }
        let mut merge: Option<usize> = None;
        let mut toggle_year: Option<i32> = None;
        let dest_root = PathBuf::from(self.organize.dest.trim());
        let mut row = 0usize;
        for (year, idxs) in years.iter().rev() {
            let n_photos: usize = idxs.iter().map(|&i| self.organize.events[i].photos.len()).sum();
            let collapsed = self.organize.collapsed_years.contains(year);
            let (hr, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
            let h = ui.ctx().animate_bool_with_time(Id::unique(("yr-h", *year)), resp.hovered(), 0.12);
            let p = ui.painter();
            p.rect_filled(hr, CornerRadius::same(8), with_alpha(Color32::WHITE, 0.02 + 0.03 * h));
            let t = ui.ctx().animate_bool_with_time(Id::unique(("yr-o", *year)), !collapsed, 0.18);
            let c = pos2(hr.left() + 14.0, hr.center().y);
            let rot = egui::emath::Rot2::from_angle(t * std::f32::consts::FRAC_PI_2);
            p.add(egui::Shape::line([vec2(-2.5, -4.0), vec2(2.5, 0.0), vec2(-2.5, 4.0)].iter().map(|v| c + rot * *v).collect(), Stroke::new(1.6, MUTED)));
            p.text(pos2(hr.left() + 30.0, hr.center().y), Align2::LEFT_CENTER, format!("📁 {year}"), FontId::proportional(16.0), TEXT);
            p.text(
                pos2(hr.left() + 110.0, hr.center().y),
                Align2::LEFT_CENTER,
                format!("{} event{} · {} photos", idxs.len(), if idxs.len() == 1 { "" } else { "s" }, fmt_count(n_photos as u64)),
                FontId::proportional(12.5),
                MUTED,
            );
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                toggle_year = Some(*year);
            }
            ui.add_space(4.0);
            if collapsed {
                continue;
            }
            for &i in idxs.iter().rev() {
                row += 1;
                let ra = ease_out_cubic(((now - self.organize.shown_at - row.min(20) as f64 * 0.03) / 0.3).clamp(0.0, 1.0) as f32);
                if ra < 1.0 {
                    ui.ctx().request_repaint();
                }
                // Camera / format breakdown for this event.
                let mut breakdown: BTreeMap<String, BTreeMap<Format, usize>> = BTreeMap::new();
                for &pi in &self.organize.events[i].photos {
                    let ph = &scan.photos[pi];
                    *breakdown.entry(organize::camera_folder(&self.organize.opts, &ph.camera)).or_default().entry(ph.format).or_default() += 1;
                }
                let thumbs: Vec<String> = {
                    let e = &self.organize.events[i];
                    let cands: Vec<&organize::Photo> = e.photos.iter().map(|&k| &scan.photos[k]).filter(|p| !p.reject && crate::filetype::is_decodable_image(&p.path.to_string_lossy().to_lowercase())).collect();
                    let step = (cands.len() / 6).max(1);
                    cands.iter().step_by(step).take(6).map(|p| p.path.to_string_lossy().into_owned()).collect()
                };
                let include = self.organize.events[i].include;
                let rejects: Vec<usize> = if self.organize.opts.rejects {
                    self.organize.events[i].photos.iter().copied().filter(|&k| scan.photos[k].reject && scan.photos[k].format != Format::Raw).collect()
                } else {
                    Vec::new()
                };
                let reviewing = self.organize.review.contains(&i);
                let mut toggle_review = false;
                let mut unreject: Option<usize> = None;
                let frame = egui::Frame::new()
                    .fill(if include { SURFACE } else { with_alpha(SURFACE, 0.5) })
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .multiply_with_opacity(ra * if include { 1.0 } else { 0.6 });
                frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let e = &mut self.organize.events[i];
                        theme::check(ui, &mut e.include, pal.accent, Id::unique(("ev-in", i, e.start.and_utc().timestamp())));
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(e.start.format("%Y-%m-%d").to_string()).size(15.0).color(pal.accent2).monospace());
                                ui.add(egui::TextEdit::singleline(&mut e.name).desired_width(300.0).font(FontId::proportional(15.0)).hint_text("Event name"));
                                ui.label(egui::RichText::new(format!("{} photos", fmt_count(e.photos.len() as u64))).size(12.5).color(MUTED));
                            });
                            ui.label(egui::RichText::new(date_range(e)).size(11.5).color(FAINT));
                            ui.horizontal_wrapped(|ui| {
                                for (cam, fmts) in &breakdown {
                                    let parts: Vec<String> = fmts.iter().map(|(f, n)| format!("{} {}", f.folder(), n)).collect();
                                    ui.label(egui::RichText::new(format!("{cam}: {}", parts.join(" · "))).size(11.5).color(TEXT).background_color(SURFACE3));
                                }
                            });
                            if e.days() > 1 || !rejects.is_empty() {
                                ui.horizontal(|ui| {
                                    if e.days() > 1 {
                                        theme::check(ui, &mut e.by_day, pal.accent2, Id::unique(("ev-day", i, e.start.and_utc().timestamp())));
                                        ui.label(egui::RichText::new(format!("Split into {} day folders", e.days())).size(12.0).color(MUTED));
                                        ui.add_space(12.0);
                                    }
                                    if !rejects.is_empty() {
                                        let t = format!("🗑 {} reject{} · {}", rejects.len(), if rejects.len() == 1 { "" } else { "s" }, if reviewing { "hide" } else { "review" });
                                        if ui.add(egui::Label::new(egui::RichText::new(t).size(12.0).color(RED)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                            toggle_review = true;
                                        }
                                    }
                                });
                            }
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("🏷").size(12.0).color(FAINT));
                                ui.add(egui::TextEdit::singleline(&mut e.tags).desired_width(260.0).hint_text("tags for this event").font(FontId::proportional(12.5)));
                                let path = dest_root.join(e.start.format("%Y").to_string()).join(e.folder());
                                let days = if e.by_day { format!("/Day 1 - {} …", e.start.format("%Y-%m-%d")) } else { String::new() };
                                ui.label(egui::RichText::new(format!("→ {}{days}", shorten_home(&path.to_string_lossy()))).size(11.5).color(FAINT));
                            });
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if i > 0 && theme::ghost_button(ui, "⤒ Merge up").on_hover_text("Merge into the event above").clicked() {
                                merge = Some(i);
                            }
                            // Thumbnail strip (keepers only).
                            for path in thumbs.iter().rev() {
                                let (r, _) = ui.allocate_exact_size(vec2(58.0, 58.0), Sense::hover());
                                ui.painter().rect_filled(r, CornerRadius::same(8), SURFACE2);
                                if let Some((tex, size, fade)) = self.thumbs.get(ui.ctx(), path) {
                                    // Center-crop to a square.
                                    let s = size.x.min(size.y);
                                    let uv = Rect::from_center_size(pos2(0.5, 0.5), vec2(s / size.x, s / size.y));
                                    ui.painter().add(egui::epaint::RectShape::filled(r, 8, with_alpha(Color32::WHITE, fade)).with_texture(tex, uv));
                                }
                            }
                        });
                    });
                });
                if reviewing && !rejects.is_empty() {
                    egui::Frame::new().fill(with_alpha(RED, 0.06)).stroke(Stroke::new(1.0, with_alpha(RED, 0.25))).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("Click a photo to keep it (its RAW twin follows).").size(12.0).color(MUTED));
                        ui.horizontal_wrapped(|ui| {
                            for &k in rejects.iter().take(120) {
                                let path = scan.photos[k].path.to_string_lossy().into_owned();
                                let (r, resp) = ui.allocate_exact_size(vec2(96.0, 96.0), Sense::click());
                                let h = ui.ctx().animate_bool_with_time(Id::unique(("rej-h", k)), resp.hovered(), 0.12);
                                ui.painter().rect_filled(r, CornerRadius::same(8), SURFACE2);
                                if let Some((tex, size, fade)) = self.thumbs.get(ui.ctx(), &path) {
                                    let s = size.x.min(size.y);
                                    let uv = Rect::from_center_size(pos2(0.5, 0.5), vec2(s / size.x, s / size.y));
                                    ui.painter().add(egui::epaint::RectShape::filled(r, 8, with_alpha(Color32::WHITE, fade)).with_texture(tex, uv));
                                }
                                ui.painter().rect_stroke(r, CornerRadius::same(8), Stroke::new(1.0 + h, lerp_color(with_alpha(RED, 0.5), GREEN, h)), StrokeKind::Inside);
                                if h > 0.0 {
                                    ui.painter().text(r.center_bottom() - vec2(0.0, 12.0), Align2::CENTER_CENTER, "Keep", FontId::proportional(12.0), with_alpha(Color32::WHITE, h));
                                }
                                let q = scan.photos[k].quality.map(|q| format!("sharpness {:.0} · brightness {:.0}%", q.sharpness, q.brightness * 100.0)).unwrap_or_default();
                                if resp.on_hover_text(format!("{}\n{q}", shorten_home(&path))).clicked() {
                                    unreject = Some(k);
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
                if toggle_review {
                    if !self.organize.review.remove(&i) {
                        self.organize.review.insert(i);
                    }
                }
                if let Some(k) = unreject {
                    organize::set_reject(scan, k, false);
                }
                ui.add_space(6.0);
            }
            ui.add_space(6.0);
        }
        if let Some(y) = toggle_year {
            if !self.organize.collapsed_years.remove(&y) {
                self.organize.collapsed_years.insert(y);
            }
        }
        if let Some(i) = merge {
            organize::merge_with_previous(&mut self.organize.events, i);
            self.organize.review.clear();
        }

        // ---------- Action bar ----------
        ui.add_space(8.0);
        let report = self.organize.report.clone();
        let running = self.organize.run_job.running();
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if running {
                let (d, t) = (self.organize.progress.done.load(Ordering::Relaxed), self.organize.progress.total.load(Ordering::Relaxed));
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("Organizing · {} / {} files · {}", fmt_count(d), fmt_count(t), human(self.organize.progress.bytes.load(Ordering::Relaxed)))).size(14.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::ghost_button(ui, "Stop").clicked() {
                            self.organize.progress.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                });
                theme::progress_bar(ui, Some(d as f32 / t.max(1) as f32), pal, 8.0);
                return;
            }
            if let Some(r) = &report {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("✔").size(18.0).color(GREEN));
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(format!("Organized {} files", fmt_count(r.created as u64))).size(16.0).strong());
                        let mut bits = vec![human(r.bytes)];
                        if r.already_there > 0 {
                            bits.push(format!("{} already there (skipped)", fmt_count(r.already_there as u64)));
                        }
                        if !r.failed.is_empty() {
                            bits.push(format!("{} failed", r.failed.len()));
                        }
                        ui.label(egui::RichText::new(bits.join(" · ")).size(12.5).color(MUTED));
                        if let Some((p, e)) = r.failed.first() {
                            ui.label(egui::RichText::new(format!("{}: {e}", shorten_home(p))).size(11.5).color(RED));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(m) = r.manifest.clone() {
                            if theme::ghost_button(ui, "↶ Undo").on_hover_text("Delete only the files this run created").clicked() {
                                self.organize.undo_job.start(ui.ctx(), move || organize::undo(&m).map_err(|e| e.to_string()));
                            }
                        }
                        if theme::primary_button(ui, "Open folder", pal).clicked() {
                            Self::open_path(self.organize.dest.trim());
                        }
                    });
                });
                return;
            }
            let free = free_space(self.organize.dest.trim());
            let needs = if self.organize.opts.symlink { 0 } else { bytes };
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    let verb = if self.organize.opts.symlink { "link" } else { "copy" };
                    ui.label(egui::RichText::new(format!("Ready to {verb} {} files · {}", fmt_count(ops.len() as u64), human(bytes))).size(16.0).strong());
                    ui.label(egui::RichText::new(format!("into {}", shorten_home(self.organize.dest.trim()))).size(12.5).color(MUTED));
                    let note = match free {
                        Some(f) if needs > f => (format!("⚠ Only {} free on the destination — copies on the same APFS volume are clones and use little space; elsewhere this won't fit.", human(f)), AMBER),
                        _ if self.organize.opts.symlink => ("Links use no space; keep the originals where they are.".to_string(), FAINT),
                        _ => ("Same APFS volume: copies are instant clones that share storage with the originals.".to_string(), FAINT),
                    };
                    ui.label(egui::RichText::new(note.0).size(11.5).color(note.1));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = !ops.is_empty() && !self.organize.dest.trim().is_empty();
                    if ok && theme::primary_button(ui, &format!("Organize {} files", fmt_count(ops.len() as u64)), pal).clicked() {
                        self.organize.confirm = true;
                    }
                    if let Some(m) = organize::last_manifest(Path::new(self.organize.dest.trim())) {
                        if theme::ghost_button(ui, "↶ Undo last run").clicked() {
                            self.organize.undo_job.start(ui.ctx(), move || organize::undo(&m).map_err(|e| e.to_string()));
                        }
                    }
                });
            });
        });
        ui.add_space(20.0);
    }

    fn organize_confirm(&mut self, ui: &mut Ui) {
        if !self.organize.confirm {
            return;
        }
        let Some(scan) = self.organize.scan.as_ref() else { return };
        let dest = PathBuf::from(self.organize.dest.trim());
        let ops = organize::build_ops(scan, &self.organize.events, &self.organize.opts, &dest);
        let mut go = false;
        let mut close = false;
        let modal = egui::Modal::new(Id::unique("org-confirm"))
            .frame(theme::card().inner_margin(egui::Margin::same(22)))
            .backdrop_color(Color32::from_black_alpha(150))
            .show(ui.ctx(), |ui| {
                ui.set_max_width(480.0);
                ui.label(egui::RichText::new("Organize photos?").size(18.0).strong());
                ui.add_space(6.0);
                let verb = if self.organize.opts.symlink { "linked" } else { "copied" };
                ui.label(
                    egui::RichText::new(format!(
                        "{} files will be {verb} into {}. Your originals are not moved or changed, existing files are never overwritten, and Undo removes exactly what this run creates.",
                        fmt_count(ops.len() as u64),
                        shorten_home(&dest.to_string_lossy())
                    ))
                    .color(MUTED),
                );
                if self.organize.opts.zip_raw {
                    ui.label(egui::RichText::new("RAW files will be packed into RAW.zip per camera and event.").size(12.5).color(FAINT));
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if theme::primary_button(ui, "Organize", self.pal).clicked() {
                        go = true;
                    }
                    if theme::ghost_button(ui, "Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if go {
            self.organize.confirm = false;
            self.organize.progress = Arc::new(Progress::default());
            let prog = self.organize.progress.clone();
            let symlink = self.organize.opts.symlink;
            self.organize.run_job.start(ui.ctx(), move || organize::execute(&ops, symlink, &dest, &prog).map_err(|e| format!("{e:#}")));
        } else if close || modal.should_close() {
            self.organize.confirm = false;
        }
    }
}

fn toggle(ui: &mut Ui, title: &str, sub: &str, value: &mut bool, pal: Palette) {
    ui.horizontal(|ui| {
        let (r, resp) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::click());
        if resp.clicked() {
            *value = !*value;
        }
        let t = ui.ctx().animate_bool_with_time(resp.id, *value, 0.18);
        let p = ui.painter();
        if t > 0.0 {
            theme::gradient_fill(ui.ctx(), p, r, CornerRadius::same(11), with_alpha(pal.accent, t), with_alpha(pal.accent2, t));
        }
        p.rect(r, CornerRadius::same(11), with_alpha(SURFACE3, 1.0 - t), Stroke::new(1.0, BORDER), StrokeKind::Inside);
        let x = egui::lerp(r.left() + 11.0..=r.right() - 11.0, theme::ease_out_back(t));
        p.circle_filled(pos2(x, r.center().y), 8.0, Color32::WHITE);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).size(13.5));
            ui.label(egui::RichText::new(sub).size(11.5).color(FAINT));
        });
    });
    ui.add_space(4.0);
}
