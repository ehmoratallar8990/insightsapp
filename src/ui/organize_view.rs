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

/// Full-size photo viewer for the preview.
struct Lightbox {
    /// Photo indices to step through.
    photos: Vec<usize>,
    cur: usize,
    tex: Option<(usize, egui::TextureHandle)>,
    job: Job<(usize, Option<egui::ColorImage>)>,
    loading: Option<usize>,
}

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
    /// Drill-down path in the planned-folders map.
    plan_path: Vec<String>,
    plan_anim: Option<(Rect, f64)>,
    lightbox: Option<Lightbox>,
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
            browse: true,
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
            plan_path: Vec::new(),
            plan_anim: None,
            lightbox: None,
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
        o.browse = false; // the planned-folders map takes over once there's a preview
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
        // Folder dialog results.
        match self.picker.poll() {
            Some((1, p)) => {
                self.organize.src = p;
                let ctx = ui.ctx().clone();
                self.organize_analyze(&ctx); // a new source: analyze it right away
            }
            Some((2, p)) => {
                self.organize.dest = p;
                self.organize.report = None;
            }
            Some((3, p)) => self.settings.media_scope = p,
            _ => {}
        }
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
            if std::env::var("INSIGHT_ORG_VIEW").is_ok() {
                if let (Some(scan), Some(e)) = (&self.organize.scan, self.organize.events.first()) {
                    let photos = organize::viewable(scan, &e.photos);
                    self.organize.lightbox = Some(Lightbox { photos, cur: 0, tex: None, job: Job::default(), loading: None });
                }
            }
            if let Ok(pp) = std::env::var("INSIGHT_ORG_PLAN") {
                self.organize.plan_path = pp.split('/').map(String::from).collect();
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
                    let msg = if rep.moved > 0 { format!("Moved {} files (each copy verified first)", fmt_count(rep.moved as u64)) } else { format!("Organized {} files", fmt_count(rep.placed as u64)) };
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
                egui::RichText::new("Sort camera media into Year › Event › Camera › Format. Preview and adjust everything first — every file is verified before an original is removed, and Undo puts it all back.")
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
                        if theme::primary_button(ui, if self.picker.busy() && self.picker.target == 1 { "Choosing…" } else { "Choose…" }, pal).on_hover_text("Pick the folder to organize").clicked() {
                            let start = self.organize.src.clone();
                            self.picker.open(ui.ctx(), 1, "Choose the folder with your photos", &start);
                        }
                        if theme::chip(ui, "📁 Folder map", self.organize.browse, pal.accent2, Id::unique("org-browse")).clicked() {
                            self.organize.browse = !self.organize.browse;
                        }
                    });
                    ui.end_row();
                    ui.label(egui::RichText::new("Organize into").color(MUTED));
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.organize.dest).desired_width(460.0));
                        if theme::ghost_button(ui, if self.picker.busy() && self.picker.target == 2 { "Choosing…" } else { "Choose…" }).on_hover_text("Pick where the organized library goes").clicked() {
                            let start = self.organize.dest.clone();
                            self.picker.open(ui.ctx(), 2, "Choose where to put the organized photos", &start);
                        }
                    });
                    ui.end_row();
                });
                if self.organize.browse {
                    ui.add_space(8.0);
                    let eng = self.engine.clone();
                    if self.organize.map.show(ui, &eng, pal, "org-map", &mut self.organize.src, 260.0) {
                        let ctx = ui.ctx().clone();
                        self.organize_analyze(&ctx);
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
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Transfer method").color(MUTED));
                    let methods = [organize::Mode::Safest, organize::Mode::Fast, organize::Mode::Copy];
                    let cur = methods.iter().position(|m| *m == self.organize.opts.method).unwrap_or(0);
                    let pick = theme::segmented(ui, &["🛡 Safest", "⚡ Fast", "📋 Copy"], cur, pal, Id::unique("org-method"));
                    self.organize.opts.method = methods[pick];
                });
                let (what, how) = match self.organize.opts.method {
                    organize::Mode::Safest => (
                        "Safest (recommended): back up → move → verify → remove the backup.",
                        "A verified safety clone is made first; then your original file itself is moved, so its creation date, tags and every other attribute stay exactly as they were. The moved file is checked against the original's content hash before the backup is deleted. Everything ends up in the destination.",
                    ),
                    organize::Mode::Fast => (
                        "Fast: move.",
                        "Same drive: an instant rename — the original file moves with all its metadata. Another drive: copy → verify → remove. Everything ends up in the destination.",
                    ),
                    organize::Mode::Copy => (
                        "Copy: keep the originals.",
                        "Verified copies go to the destination; your originals stay where they are. Copies are new files, so their creation date is today.",
                    ),
                };
                ui.label(egui::RichText::new(what).size(13.0).color(TEXT));
                ui.add(egui::Label::new(egui::RichText::new(how).size(11.5).color(FAINT)).wrap());
                ui.add_space(8.0);
                ui.columns(2, |c| {
                    toggle(&mut c[0], "Separate formats", "RAW / JPG / HEIC subfolders inside each camera", &mut self.organize.opts.split_formats, pal);
                    toggle(&mut c[0], "Zip RAW files", "One RAW.zip per camera per event (stored, not recompressed)", &mut self.organize.opts.zip_raw, pal);
                    if self.organize.opts.method.moves() {
                        toggle(&mut c[1], "Leave links at the old locations", "A symlink at each original path points to its new home (zipped RAWs excluded)", &mut self.organize.opts.link_back, pal);
                    }
                    toggle(&mut c[1], "Include non-camera images", "Screenshots, downloads & exports go to “Other images”", &mut self.organize.opts.include_others, pal);
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
                    ui.label(egui::RichText::new(if cfg!(target_os = "macos") { "→ Finder tags" } else if cfg!(windows) { "→ XMP sidecars (Lightroom, Capture One)" } else { "→ user.xdg.tags" }).size(11.5).color(FAINT));
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
                            "4.  Organize moves (copy → verify → remove) or copies into  2025 / 2025-07-14 Wedding / Canon EOS R8 / RAW. Undo restores everything.",
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
        let mut n_videos = 0usize;
        for p in &scan.photos {
            if p.camera.is_empty() {
                n_videos += 1; // videos without camera metadata: their own folder, not a camera
            } else {
                *cams.entry(p.camera.clone()).or_default() += 1;
            }
        }
        let ops = organize::build_ops(scan, &o.events, &o.opts, Path::new(o.dest.trim()));
        let bytes: u64 = ops.iter().map(|op| op.size).sum();
        let other_bytes: u64 = scan.others.iter().map(|x| x.1).sum();

        // ---------- Stats ----------
        let stats = [
            ("📷", fmt_count(scan.photos.len() as u64), if n_videos > 0 { format!("camera photos & videos ({n_videos} videos)") } else { "camera photos".to_string() }, pal.accent),
            ("📅", fmt_count(o.events.len() as u64), "events".to_string(), pal.accent2),
            ("🎞", fmt_count(cams.len() as u64), if cams.len() == 1 { "camera".into() } else { "cameras".into() }, GREEN),
            ("🖼", fmt_count(scan.others.len() as u64), format!("not from a camera · {} → {}", human(other_bytes), if o.opts.include_others { "Other images" } else { "left alone" }), AMBER),
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

        // ---------- Planned folders map ----------
        self.plan_map(ui, &ops, now);
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
                    let key = if ph.camera.is_empty() { "Videos".to_string() } else { organize::camera_folder(&self.organize.opts, &ph.camera) };
                    *breakdown.entry(key).or_default().entry(ph.format).or_default() += 1;
                }
                let viewable = organize::viewable(scan, &self.organize.events[i].photos);
                let thumbs: Vec<(usize, String)> = {
                    let cands: Vec<usize> = viewable.iter().copied().filter(|&k| !scan.photos[k].reject && crate::filetype::is_decodable_image(&scan.photos[k].path.to_string_lossy().to_lowercase())).collect();
                    let step = (cands.len() / 6).max(1);
                    cands.iter().step_by(step).take(6).map(|&k| (k, scan.photos[k].path.to_string_lossy().into_owned())).collect()
                };
                let mut open_viewer: Option<(Vec<usize>, usize)> = None;
                let include = self.organize.events[i].include;
                let rejects: Vec<usize> = if self.organize.opts.rejects {
                    self.organize.events[i].photos.iter().copied().filter(|&k| scan.photos[k].reject && scan.photos[k].format != Format::Raw).collect()
                } else {
                    Vec::new()
                };
                let reviewing = self.organize.review.contains(&i);
                let mut toggle_review = false;
                let unreject: Option<usize> = None;
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
                                    let text = if cam == "Videos" { format!("🎞 {} videos", fmts.values().sum::<usize>()) } else { format!("{cam}: {}", parts.join(" · ")) };
                                    ui.label(egui::RichText::new(text).size(11.5).color(TEXT).background_color(SURFACE3));
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
                            if !viewable.is_empty() && theme::ghost_button(ui, &format!("🔍 View {}", viewable.len())).on_hover_text("Browse this event's photos full size").clicked() {
                                open_viewer = Some((viewable.clone(), viewable[0]));
                            }
                            for (k, path) in thumbs.iter().rev() {
                                let (r, resp) = ui.allocate_exact_size(vec2(58.0, 58.0), Sense::click());
                                let h = ui.ctx().animate_bool_with_time(Id::unique(("th-h", *k)), resp.hovered(), 0.12);
                                let r = r.expand(2.0 * h);
                                ui.painter().rect_filled(r, CornerRadius::same(8), SURFACE2);
                                if let Some((tex, size, fade)) = self.thumbs.get(ui.ctx(), path) {
                                    // Center-crop to a square.
                                    let s = size.x.min(size.y);
                                    let uv = Rect::from_center_size(pos2(0.5, 0.5), vec2(s / size.x, s / size.y));
                                    ui.painter().add(egui::epaint::RectShape::filled(r, 8, with_alpha(Color32::WHITE, fade)).with_texture(tex, uv));
                                }
                                if h > 0.0 {
                                    ui.painter().rect_stroke(r, CornerRadius::same(8), Stroke::new(1.5, with_alpha(pal.accent2, h)), StrokeKind::Inside);
                                }
                                if resp.on_hover_cursor(egui::CursorIcon::ZoomIn).on_hover_text("Click to enlarge").clicked() {
                                    open_viewer = Some((viewable.clone(), *k));
                                }
                            }
                        });
                    });
                });
                if reviewing && !rejects.is_empty() {
                    egui::Frame::new().fill(with_alpha(RED, 0.06)).stroke(Stroke::new(1.0, with_alpha(RED, 0.25))).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("Click a photo to see it full size and keep it if you like it (its RAW twin follows).").size(12.0).color(MUTED));
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
                                    ui.painter().text(r.center_bottom() - vec2(0.0, 12.0), Align2::CENTER_CENTER, "🔍 View", FontId::proportional(12.0), with_alpha(Color32::WHITE, h));
                                }
                                let q = scan.photos[k].quality.map(|q| format!("sharpness {:.0} · brightness {:.0}%", q.sharpness, q.brightness * 100.0)).unwrap_or_default();
                                if resp.on_hover_cursor(egui::CursorIcon::ZoomIn).on_hover_text(format!("{}\n{q}", shorten_home(&path))).clicked() {
                                    open_viewer = Some((rejects.clone(), k));
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
                if let Some((list, k)) = open_viewer {
                    let cur = list.iter().position(|&x| x == k).unwrap_or(0);
                    self.organize.lightbox = Some(Lightbox { photos: list, cur, tex: None, job: Job::default(), loading: None });
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
                    ui.label(egui::RichText::new(format!("{} · {} / {} files · {}", match self.organize.opts.method { organize::Mode::Safest => "Moving safely (backup → move → verify)", organize::Mode::Fast => "Moving", organize::Mode::Copy => "Copying & verifying" }, fmt_count(d), fmt_count(t), human(self.organize.progress.bytes.load(Ordering::Relaxed)))).size(14.0));
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
                        let head = if r.moved > 0 { format!("Moved {} files into the destination", fmt_count(r.moved as u64)) } else { format!("Copied {} files", fmt_count(r.placed as u64)) };
                        ui.label(egui::RichText::new(head).size(16.0).strong());
                        let mut bits = vec![human(r.bytes), "every copy verified by content hash".to_string()];
                        if r.linked > 0 {
                            bits.push(format!("{} links left at the old locations", fmt_count(r.linked as u64)));
                        }
                        if r.removed_dirs > 0 {
                            bits.push(format!("{} emptied folders removed", r.removed_dirs));
                        }
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
                            if theme::ghost_button(ui, "↩ Undo").on_hover_text("Put every file back where it was (verified), remove links and copies this run made").clicked() {
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
            let src_root = PathBuf::from(self.organize.scanned.0.trim());
            let same_drive_free = organize::free_move(&src_root, Path::new(self.organize.dest.trim()));
            let moving = self.organize.opts.method.moves();
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    let verb = if moving { "move" } else { "copy" };
                    ui.label(egui::RichText::new(format!("Ready to {verb} {} files · {}", fmt_count(ops.len() as u64), human(bytes))).size(16.0).strong());
                    ui.label(egui::RichText::new(format!("into {}", shorten_home(self.organize.dest.trim()))).size(12.5).color(MUTED));
                    let note = match (moving, same_drive_free, free) {
                        (_, true, _) if self.organize.opts.method == organize::Mode::Safest => ("Same drive: each original is moved as-is (metadata intact) behind a free, verified safety clone — no extra space needed.".to_string(), GREEN),
                        (_, true, _) if moving => ("Same drive: originals are moved by instant renames — no extra space, metadata intact.".to_string(), GREEN),
                        (_, true, _) => ("Same drive: copies are clones that share storage with the originals until either is edited.".to_string(), FAINT),
                        (_, false, Some(f)) if bytes > f => (format!("⚠ Only {} free on the destination drive; {} needed.", human(f), human(bytes)), AMBER),
                        (true, false, _) => (format!("Other drive: needs {} there; each original is removed right after its copy verifies.", human(bytes)), FAINT),
                        _ => (format!("Needs {} on the destination drive.", human(bytes)), FAINT),
                    };
                    ui.label(egui::RichText::new(note.0).size(11.5).color(note.1));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = !ops.is_empty() && !self.organize.dest.trim().is_empty();
                    let verb = if self.organize.opts.method.moves() { "Move & organize" } else { "Copy & organize" };
                    if ok && theme::primary_button(ui, &format!("{verb} {} files", fmt_count(ops.len() as u64)), pal).clicked() {
                        self.organize.confirm = true;
                    }
                    if let Some(m) = organize::last_manifest(Path::new(self.organize.dest.trim())) {
                        if theme::ghost_button(ui, "↩ Undo last run").clicked() {
                            self.organize.undo_job.start(ui.ctx(), move || organize::undo(&m).map_err(|e| e.to_string()));
                        }
                    }
                });
            });
        });
        ui.add_space(20.0);
        self.organize_viewer(ui, scan);
    }

    /// Full-size viewer: ←/→ to step, Esc to close, keep or reject in place.
    fn organize_viewer(&mut self, ui: &mut Ui, scan: &mut Scan) {
        let pal = self.pal;
        let ctx = ui.ctx().clone();
        let Some(lb) = self.organize.lightbox.as_mut() else { return };
        if lb.photos.is_empty() {
            self.organize.lightbox = None;
            return;
        }
        lb.cur = lb.cur.min(lb.photos.len() - 1);
        let k = lb.photos[lb.cur];
        if let Some((idx, img)) = lb.job.poll() {
            lb.loading = None;
            if let Some(img) = img {
                lb.tex = Some((idx, ctx.load_texture("org-viewer", img, egui::TextureOptions::LINEAR)));
            }
        }
        if lb.tex.as_ref().is_none_or(|(i, _)| *i != k) && lb.loading != Some(k) && !lb.job.running() {
            let path = scan.photos[k].path.clone();
            lb.loading = Some(k);
            lb.job.start(&ctx, move || {
                let img = organize::preview_image(&path, 2000).map(|i| {
                    let rgba = i.to_rgba8();
                    egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw())
                });
                (k, img)
            });
        }
        let (left, right) = ctx.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight)));
        let n = lb.photos.len();
        let mut step: i64 = if left { -1 } else if right { 1 } else { 0 };
        let mut close = false;
        let mut toggle_reject = false;
        let screen = ctx.content_rect();
        let w = (screen.width() * 0.86).max(400.0);
        let h = (screen.height() * 0.84).max(300.0);
        let photo = &scan.photos[k];
        let rejected = photo.reject;
        let name = photo.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let info = format!(
            "{} · {} · {}{}",
            photo.taken.format("%a %b %-d, %Y %H:%M"),
            if photo.camera.is_empty() { "Video" } else { photo.camera.as_str() },
            human(photo.size),
            photo.quality.map(|q| format!(" · sharpness {:.0}", q.sharpness)).unwrap_or_default()
        );
        let modal = egui::Modal::new(Id::unique("org-viewer"))
            .frame(egui::Frame::new().fill(BG).stroke(Stroke::new(1.0, BORDER)).corner_radius(CornerRadius::same(16)).inner_margin(egui::Margin::same(16)))
            .backdrop_color(Color32::from_black_alpha(200))
            .show(&ctx, |ui| {
                ui.set_width(w);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&name).size(16.0).strong());
                        ui.label(egui::RichText::new(&info).size(12.0).color(MUTED));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::ghost_button(ui, "✖ Close").clicked() {
                            close = true;
                        }
                        let label = if rejected { "✔ Keep this photo" } else { "🗑 Move to Rejects" };
                        let p = if rejected { Palette { accent: GREEN, accent2: Color32::from_rgb(110, 231, 183) } } else { Palette { accent: RED, accent2: Color32::from_rgb(251, 113, 133) } };
                        if theme::primary_button(ui, label, p).clicked() {
                            toggle_reject = true;
                        }
                        ui.label(egui::RichText::new(format!("{} / {}", lb.cur + 1, n)).size(12.5).color(FAINT));
                        if rejected {
                            ui.label(egui::RichText::new("REJECT").size(11.0).strong().color(RED));
                        }
                    });
                });
                ui.add_space(8.0);
                let (area, _) = ui.allocate_exact_size(vec2(w, h - 70.0), Sense::hover());
                ui.painter().rect_filled(area, CornerRadius::same(10), SURFACE);
                match lb.tex.as_ref().filter(|(i, _)| *i == k) {
                    Some((_, tex)) => {
                        let size = tex.size_vec2();
                        let scale = (area.width() / size.x).min(area.height() / size.y);
                        let r = Rect::from_center_size(area.center(), size * scale);
                        ui.painter().image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                    }
                    None => {
                        ui.painter().text(area.center(), Align2::CENTER_CENTER, if lb.loading == Some(k) { "Loading…" } else { "No preview for this format" }, FontId::proportional(15.0), FAINT);
                        ctx.request_repaint_after(std::time::Duration::from_millis(60));
                    }
                }
                // Prev / next zones.
                for (dir, rect, glyph) in [(-1i64, Rect::from_min_max(area.min, pos2(area.left() + 70.0, area.bottom())), "‹"), (1, Rect::from_min_max(pos2(area.right() - 70.0, area.top()), area.max), "›")] {
                    let resp = ui.interact(rect, Id::unique(("viewer-nav", dir)), Sense::click());
                    let hv = ui.ctx().animate_bool_with_time(Id::unique(("viewer-nav-h", dir)), resp.hovered(), 0.12);
                    ui.painter().circle_filled(rect.center(), 22.0, with_alpha(Color32::BLACK, 0.35 + 0.3 * hv));
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(30.0), with_alpha(Color32::WHITE, 0.6 + 0.4 * hv));
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        step = dir;
                    }
                }
                ui.add_space(6.0);
                ui.label(egui::RichText::new("← / → previous · next    Esc close").size(11.5).color(FAINT));
                let _ = pal;
            });
        if toggle_reject {
            organize::set_reject(scan, k, !rejected);
        }
        if close || modal.should_close() {
            self.organize.lightbox = None;
        }
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
                let text = if self.organize.opts.method.moves() {
                    format!(
                        "{} files will be moved into {}. Each one is copied, read back and checked against the original's content hash; only then is the original removed{}. Existing files are never overwritten, and Undo puts everything back where it was.",
                        fmt_count(ops.len() as u64),
                        shorten_home(&dest.to_string_lossy()),
                        if self.organize.opts.link_back { " and a link left in its place" } else { "" }
                    )
                } else {
                    format!(
                        "{} files will be copied into {} and verified. Originals stay where they are; existing files are never overwritten, and Undo removes exactly what this run creates.",
                        fmt_count(ops.len() as u64),
                        shorten_home(&dest.to_string_lossy())
                    )
                };
                ui.label(egui::RichText::new(text).color(MUTED));
                if self.organize.opts.zip_raw {
                    ui.label(egui::RichText::new("RAW files will be packed into RAW.zip per camera and event.").size(12.5).color(FAINT));
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if theme::primary_button(ui, if self.organize.opts.method.moves() { "Move & organize" } else { "Copy & organize" }, self.pal).clicked() {
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
            let mode = self.organize.opts.method;
            let link_back = self.organize.opts.link_back;
            let src_root = PathBuf::from(self.organize.scanned.0.trim());
            self.organize.run_job.start(ui.ctx(), move || organize::execute(&ops, mode, link_back, &src_root, &dest, &prog).map_err(|e| format!("{e:#}")));
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


/// The folder tree the plan will create (sizes + file counts), for the map.
#[derive(Default)]
struct PlanNode {
    bytes: u64,
    files: usize,
    kids: BTreeMap<String, PlanNode>,
}

impl PlanNode {
    fn build(ops: &[organize::Op], dest: &Path) -> PlanNode {
        let mut root = PlanNode::default();
        for op in ops {
            let Ok(rel) = op.dst.strip_prefix(dest) else { continue };
            // Folders only: the file (or RAW.zip member) itself is aggregated.
            let comps: Vec<String> = rel.parent().map(|p| p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect()).unwrap_or_default();
            let mut n = &mut root;
            n.bytes += op.size;
            n.files += 1;
            for c in comps {
                n = n.kids.entry(c).or_default();
                n.bytes += op.size;
                n.files += 1;
            }
            if op.zip {
                let z = n.kids.entry("RAW.zip".into()).or_default();
                z.bytes += op.size;
                z.files += 1;
            }
        }
        root
    }
}

impl InsightApp {
    /// Zoomable treemap of the destination structure (like Storage's space map).
    fn plan_map(&mut self, ui: &mut Ui, ops: &[organize::Op], now: f64) {
        let pal = self.pal;
        let dest = PathBuf::from(self.organize.dest.trim());
        let root = PlanNode::build(ops, &dest);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Resolve the drill-down path (names may change as events are renamed).
            let mut node = &root;
            let mut depth = 0;
            for name in &self.organize.plan_path {
                match node.kids.get(name) {
                    Some(k) => {
                        node = k;
                        depth += 1;
                    }
                    None => break,
                }
            }
            self.organize.plan_path.truncate(depth);
            let mut go: Option<usize> = None;
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(4.0, 18.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(2), pal.accent);
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Planned folders").size(16.0).strong());
                ui.add_space(10.0);
                let root_name = dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Destination".into());
                let mut crumbs = vec![root_name];
                crumbs.extend(self.organize.plan_path.iter().cloned());
                let n = crumbs.len();
                for (j, c) in crumbs.into_iter().enumerate() {
                    let last = j + 1 == n;
                    if ui.add(egui::Label::new(egui::RichText::new(c).size(13.0).color(if last { TEXT } else { MUTED })).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        go = Some(j);
                    }
                    if !last {
                        ui.label(egui::RichText::new("›").color(FAINT));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(format!("{} files · {}", fmt_count(node.files as u64), human(node.bytes))).size(12.5).color(pal.accent2));
                });
            });
            ui.label(egui::RichText::new("What Organize will create — click a folder to zoom in, use the breadcrumbs to go back. Updates live as you edit.").size(11.5).color(FAINT));
            ui.add_space(6.0);
            let (area, _) = ui.allocate_exact_size(vec2(ui.available_width(), 260.0), Sense::hover());
            let names: Vec<(&String, &PlanNode)> = node.kids.iter().collect();
            let mut items: Vec<(u32, u64)> = names.iter().enumerate().map(|(i, (_, k))| (i as u32, k.bytes.max(1))).collect();
            items.sort_by(|a, b| b.1.cmp(&a.1));
            let in_kids: usize = node.kids.values().map(|k| k.files).sum();
            let loose = node.files.saturating_sub(in_kids);
            if loose > 0 {
                let loose_bytes = node.bytes.saturating_sub(node.kids.values().map(|k| k.bytes).sum());
                items.push((crate::index::NONE, loose_bytes.max(1)));
            }
            let tiles = super::storage_view::squarify(&items, area.shrink(1.0));
            let (t_anim, from) = match self.organize.plan_anim {
                Some((r, start)) => {
                    let t = ease_out_cubic(((now - start) / 0.4) as f32);
                    if t >= 1.0 {
                        self.organize.plan_anim = None;
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
                    super::storage_view::lerp_rect(Rect::from_min_max(f.min + a * f.size(), f.min + b * f.size()), r, t_anim)
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
            let mut drill: Option<(String, Rect)> = None;
            for (i, t) in tiles.iter().enumerate() {
                let r = map(t.rect).shrink(1.5);
                if r.width() < 1.0 || r.height() < 1.0 {
                    continue;
                }
                let (name, files, has_kids, base) = if t.id == crate::index::NONE {
                    ("Files".to_string(), loose, false, kind_color(crate::filetype::Kind::Image))
                } else {
                    let (n, k) = names[t.id as usize];
                    let base = match n.as_str() {
                        "Rejects" => RED,
                        "RAW" | "RAW.zip" => Color32::from_rgb(245, 158, 11),
                        "JPG" => Color32::from_rgb(56, 140, 255),
                        "HEIC" => Color32::from_rgb(20, 184, 166),
                        _ => lerp_color(TONES[i % TONES.len()], pal.accent, 0.15),
                    };
                    (n.clone(), k.files, !k.kids.is_empty(), base)
                };
                let resp = ui.interact(t.rect, Id::unique(("plan-tile", depth, i)), Sense::click());
                let h = ui.ctx().animate_bool_with_time(Id::unique(("plan-h", depth, name.as_str())), resp.hovered(), 0.12);
                p.rect_filled(r, CornerRadius::same(6), lerp_color(with_alpha(base, 0.5), with_alpha(base, 0.85), h));
                p.rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, with_alpha(Color32::WHITE, 0.08 + 0.25 * h)), StrokeKind::Inside);
                if r.width() > 56.0 && r.height() > 26.0 && t_anim > 0.6 {
                    let mut job = egui::text::LayoutJob::single_section(name.clone(), egui::TextFormat::simple(FontId::proportional(12.5), Color32::WHITE));
                    job.wrap = egui::text::TextWrapping { max_width: r.width() - 12.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                    let g = ui.painter().layout_job(job);
                    p.galley(r.min + vec2(6.0, 5.0), g, Color32::WHITE);
                    if r.height() > 42.0 {
                        p.text(r.min + vec2(6.0, 22.0), Align2::LEFT_TOP, format!("{} files · {}", fmt_count(files as u64), human(t.size)), FontId::proportional(11.0), with_alpha(Color32::WHITE, 0.75));
                    }
                    if has_kids && r.height() > 42.0 {
                        p.text(r.right_top() + vec2(-8.0, 6.0), Align2::RIGHT_TOP, "📁", FontId::proportional(11.0), with_alpha(Color32::WHITE, 0.6));
                    }
                }
                if resp.hovered() {
                    egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), Id::unique("plan-tip"), egui::PopupAnchor::Pointer).show(|ui| {
                        ui.label(format!("{name}\n{} files · {}{}", fmt_count(files as u64), human(t.size), if has_kids { "\nclick to open" } else { "" }));
                    });
                }
                if resp.clicked() && has_kids {
                    drill = Some((name, t.rect));
                }
            }
            if tiles.is_empty() {
                ui.painter().text(area.center(), Align2::CENTER_CENTER, "Nothing selected to organize", FontId::proportional(14.0), FAINT);
            }
            if let Some((name, r)) = drill {
                self.organize.plan_path.push(name);
                self.organize.plan_anim = Some((r, now));
            }
            if let Some(j) = go {
                self.organize.plan_path.truncate(j);
                self.organize.plan_anim = Some((area.shrink(area.width().min(area.height()) * 0.25), now));
            }
        });
    }
}
