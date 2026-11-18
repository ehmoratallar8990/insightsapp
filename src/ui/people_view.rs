//! People & Pets: find faces, name them with a relationship, teach pets by example,
//! browse everyone's photos. Everything stays on this computer.

use super::app::{InsightApp, fmt_count};
use super::cleanup_view::Job;
use super::theme::{self, *};
use crate::faces::{self, Kind, PeopleStore};
use egui::{Align2, Color32, CornerRadius, FontId, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Background-loaded face crops (by path + box).
#[derive(Default)]
pub struct FaceThumbs {
    tex: HashMap<String, egui::TextureHandle>,
    pending: HashSet<String>,
    rx: Option<std::sync::mpsc::Receiver<(String, Option<egui::ColorImage>)>>,
    tx: Option<std::sync::mpsc::Sender<(String, Option<egui::ColorImage>)>>,
}

impl FaceThumbs {
    pub fn get(&mut self, ctx: &egui::Context, path: &str, rel: [f32; 4]) -> Option<egui::TextureId> {
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        while let Ok((k, img)) = self.rx.as_ref()?.try_recv() {
            if let Some(img) = img {
                let t = ctx.load_texture(&k, img, egui::TextureOptions::LINEAR);
                self.tex.insert(k.clone(), t);
            }
        }
        let key = format!("{path}#{:.4},{:.4}", rel[0], rel[1]);
        if let Some(t) = self.tex.get(&key) {
            return Some(t.id());
        }
        if self.pending.len() < 64 && self.pending.insert(key.clone()) {
            let tx = self.tx.clone()?;
            let (p, ctx) = (PathBuf::from(path), ctx.clone());
            rayon::spawn(move || {
                let img = faces::face_crop(&p, rel, 128).map(|i| egui::ColorImage::from_rgba_unmultiplied([128, 128], i.as_raw()));
                let _ = tx.send((key, img));
                ctx.request_repaint();
            });
        }
        None
    }
}

pub struct PeopleUi {
    pub scope: String,
    progress: Arc<faces::Progress>,
    job: Job<Result<usize, String>>,
    dl: Arc<std::sync::atomic::AtomicU64>,
    dl_total: Arc<std::sync::atomic::AtomicU64>,
    pub selected: Option<u32>,
    /// Name / relation drafts per unnamed cluster (keyed by its first face index).
    drafts: HashMap<usize, (String, String)>,
    new_pet: (String, String),
    edit: Option<(u32, String, String)>,
    pub thumbs: FaceThumbs,
    clusters: Option<(usize, Vec<Vec<usize>>)>,
    shown_at: f64,
}

impl PeopleUi {
    pub fn new(scope: String) -> Self {
        PeopleUi {
            scope,
            progress: Arc::new(faces::Progress::default()),
            job: Job::default(),
            dl: Default::default(),
            dl_total: Default::default(),
            selected: None,
            drafts: HashMap::new(),
            new_pet: (String::new(), "Pet".into()),
            edit: None,
            thumbs: FaceThumbs::default(),
            clusters: None,
            shown_at: 0.0,
        }
    }
}

fn relation_picker(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, rel: &mut String) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(if rel.is_empty() { "Relationship" } else { rel.as_str() })
        .width(120.0)
        .show_ui(ui, |ui| {
            for r in faces::RELATIONS {
                ui.selectable_value(rel, r.to_string(), *r);
            }
        });
}

/// Draw a round avatar (face crop or photo thumbnail).
fn avatar(ui: &Ui, rect: Rect, tex: Option<egui::TextureId>, fallback: &str, ring: Color32) {
    let p = ui.painter();
    p.circle_filled(rect.center(), rect.width() / 2.0, SURFACE3);
    if let Some(t) = tex {
        p.add(egui::epaint::RectShape::filled(rect, (rect.width() / 2.0) as u8, Color32::WHITE).with_texture(t, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))));
    } else {
        p.text(rect.center(), Align2::CENTER_CENTER, fallback, FontId::proportional(rect.width() * 0.4), MUTED);
    }
    p.circle_stroke(rect.center(), rect.width() / 2.0, Stroke::new(2.0, ring));
}

impl InsightApp {
    fn people_start_scan(&mut self, ctx: &egui::Context) {
        let items = crate::media::candidates_with(&self.engine.index.read(), &self.people.scope, |n| {
            crate::filetype::is_decodable_image(n) || n.ends_with(".cr3")
        });
        let prog = Arc::new(faces::Progress::default());
        self.people.progress = prog.clone();
        let (dl, dl_total) = (self.people.dl.clone(), self.people.dl_total.clone());
        self.people.clusters = None;
        self.people.job.start(ctx, move || {
            if !faces::models_ready() {
                faces::download(|d, t| {
                    dl.store(d, Ordering::Relaxed);
                    dl_total.store(t, Ordering::Relaxed);
                })
                .map_err(|e| format!("{e:#}"))?;
            }
            let m = faces::Models::load().map_err(|e| format!("{e:#}"))?;
            faces::scan(&m, items, faces::shared(), &prog).map_err(|e| format!("{e:#}"))
        });
    }

    pub fn people_page(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let now = ui.input(|i| i.time);
        if let Some(r) = self.people.job.poll() {
            match r {
                Ok(n) => self.toast(ui, format!("Found {} faces", fmt_count(n as u64))),
                Err(e) => self.toast(ui, format!("Face scan failed: {e}")),
            }
            self.people.clusters = None;
        }
        if self.picker.target == 4 {
            if let Some((_, p)) = self.picker.poll() {
                self.people.scope = p;
            }
        }
        let running = self.people.job.running();
        if running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        let store = faces::shared().clone();
        // Test hook: INSIGHT_PET="Mushu:Pet:/a.jpg,/b.jpg" creates a pet with examples once.
        if let Ok(spec) = std::env::var("INSIGHT_PET") {
            let done = Id::unique("pet-hook");
            if !ui.ctx().data(|d| d.get_temp::<bool>(done).unwrap_or(false)) {
                ui.ctx().data_mut(|d| d.insert_temp(done, true));
                let parts: Vec<&str> = spec.splitn(3, ':').collect();
                if parts.len() == 3 {
                    let mut st = store.write();
                    let id = match st.subjects.iter().find(|x| x.name == parts[0]).map(|x| x.id) {
                        Some(id) => id,
                        None => st.add_subject(parts[0], parts[1], Kind::Pet),
                    };
                    if let Some(x) = st.subject_mut(id) {
                        x.examples = parts[2].split(',').map(String::from).collect();
                    }
                    st.save();
                    self.people.selected = Some(id);
                }
            }
        }
        theme::debug_scroll(egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("people-scroll")).show(ui, |ui| {
            ui.label(egui::RichText::new("People & Pets").size(26.0).strong());
            ui.label(
                egui::RichText::new("Name the people and animals in your photos, then find them anywhere: person:Jose · who:dad · with:Mushu. Recognition runs on this computer.")
                    .color(MUTED),
            );
            ui.add_space(14.0);

            // ---------- Scan ----------
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Find faces in").color(MUTED));
                    ui.add(egui::TextEdit::singleline(&mut self.people.scope).desired_width(340.0));
                    if theme::ghost_button(ui, "Choose…").clicked() {
                        let s = self.people.scope.clone();
                        self.picker.open(ui.ctx(), 4, "Choose a folder of photos", &s);
                    }
                    if running {
                        theme::spinner(ui, 18.0, pal.accent2);
                        let (d, t, f) = (self.people.progress.done.load(Ordering::Relaxed), self.people.progress.total.load(Ordering::Relaxed), self.people.progress.faces.load(Ordering::Relaxed));
                        let dl = self.people.dl.load(Ordering::Relaxed);
                        let txt = if t == 0 && dl > 0 {
                            format!("Downloading face models · {} / {}", crate::storage::human(dl), crate::storage::human(self.people.dl_total.load(Ordering::Relaxed)))
                        } else {
                            format!("{} / {} photos · {} faces", fmt_count(d), fmt_count(t), fmt_count(f))
                        };
                        ui.label(egui::RichText::new(txt).color(MUTED));
                        if theme::ghost_button(ui, "Stop").clicked() {
                            self.people.progress.cancel.store(true, Ordering::Relaxed);
                        }
                    } else {
                        let label = if faces::models_ready() { "Find faces" } else { "Download models (37 MB) & find faces" };
                        if theme::primary_button(ui, label, pal).clicked() {
                            let ctx = ui.ctx().clone();
                            self.people_start_scan(&ctx);
                        }
                    }
                });
                if running {
                    let (d, t) = (self.people.progress.done.load(Ordering::Relaxed), self.people.progress.total.load(Ordering::Relaxed));
                    theme::progress_bar(ui, (t > 0).then(|| d as f32 / t as f32), pal, 6.0);
                }
                let s = store.read();
                let photos: HashSet<&String> = s.faces.iter().map(|f| &f.path).collect();
                let named = s.faces.iter().filter(|f| f.subject.is_some()).count();
                ui.label(
                    egui::RichText::new(format!(
                        "{} faces in {} photos · {} recognised · {} people & pets named · already-scanned photos are skipped",
                        fmt_count(s.faces.len() as u64),
                        fmt_count(photos.len() as u64),
                        fmt_count(named as u64),
                        s.subjects.len()
                    ))
                    .size(12.0)
                    .color(FAINT),
                );
            });
            ui.add_space(14.0);

            // ---------- Named ----------
            self.people_named(ui, &store, now);
            ui.add_space(14.0);

            // ---------- Selected subject ----------
            if let Some(id) = self.people.selected {
                self.people_detail(ui, &store, id);
                ui.add_space(14.0);
            }

            // ---------- Unnamed groups ----------
            self.people_unnamed(ui, &store);
            ui.add_space(14.0);

            // ---------- Add a pet ----------
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section_title(ui, "Add a pet (or any recurring subject)", pal);
                ui.label(egui::RichText::new("Faces don't work for animals: give it a name, then mark a few photos of it — Insight finds look-alikes with the on-device CLIP model (Media tab must have analysed those photos).").size(12.0).color(MUTED));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.people.new_pet.0).hint_text("Name, e.g. Mushu").desired_width(200.0));
                    relation_picker(ui, "new-pet-rel", &mut self.people.new_pet.1);
                    if theme::primary_button(ui, "Add", pal).clicked() && !self.people.new_pet.0.trim().is_empty() {
                        let mut s = store.write();
                        let id = s.add_subject(&self.people.new_pet.0, &self.people.new_pet.1, Kind::Pet);
                        s.save();
                        self.people.selected = Some(id);
                        self.people.new_pet.0.clear();
                    }
                });
            });
            ui.add_space(20.0);
        });
        let _ = now;
    }

    fn people_named(&mut self, ui: &mut Ui, store: &Arc<parking_lot::RwLock<PeopleStore>>, now: f64) {
        let pal = self.pal;
        let subjects = store.read().subjects.clone();
        theme::section_title(ui, "Your people & pets", pal);
        if subjects.is_empty() {
            ui.label(egui::RichText::new("Nobody named yet — find faces, then name a group below, or add a pet.").color(FAINT));
            return;
        }
        let media = self.media.store.clone();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(14.0, 14.0);
            for (i, s) in subjects.iter().enumerate() {
                let a = theme::appear(ui, self.people.shown_at.max(self.page_changed_at), 0.03 * i as f64);
                let (r, resp) = ui.allocate_exact_size(vec2(110.0, 142.0), Sense::click());
                let hov = ui.ctx().animate_bool_with_time(Id::unique(("subj-h", s.id)), resp.hovered() || self.people.selected == Some(s.id), 0.12);
                let p = ui.painter();
                p.rect_filled(r, CornerRadius::same(14), lerp_color(SURFACE, SURFACE2, hov));
                p.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, lerp_color(BORDER, pal.accent, hov)), StrokeKind::Inside);
                let ar = Rect::from_center_size(pos2(r.center().x, r.top() + 46.0), vec2(72.0, 72.0) * (0.9 + 0.1 * a));
                let (tex, count) = match s.kind {
                    Kind::Person => {
                        let st = store.read();
                        let fs = st.faces_of(s.id);
                        let best = fs.iter().max_by(|a, b| st.faces[**a].score.total_cmp(&st.faces[**b].score)).copied();
                        let tex = best.and_then(|i| {
                            let f = &st.faces[i];
                            let (path, rel) = (f.path.clone(), f.rel);
                            drop(st);
                            self.people.thumbs.get(ui.ctx(), &path, rel)
                        });
                        let n = store.read().photos_of(s.id, &media.read()).len();
                        (tex, n)
                    }
                    Kind::Pet => {
                        let photos = store.read().photos_of(s.id, &media.read());
                        let tex = photos.first().and_then(|p| self.thumbs.get(ui.ctx(), p).map(|t| t.0));
                        (tex, photos.len())
                    }
                };
                avatar(ui, ar, tex, if s.kind == Kind::Pet { "🐾" } else { "👤" }, if s.kind == Kind::Pet { AMBER } else { pal.accent2 });
                let p = ui.painter();
                p.text(pos2(r.center().x, r.top() + 98.0), Align2::CENTER_CENTER, &s.name, FontId::proportional(14.0), TEXT);
                p.text(pos2(r.center().x, r.top() + 116.0), Align2::CENTER_CENTER, if s.relation.is_empty() { "—" } else { s.relation.as_str() }, FontId::proportional(11.5), pal.accent2);
                p.text(pos2(r.center().x, r.top() + 132.0), Align2::CENTER_CENTER, format!("{} photos", fmt_count(count as u64)), FontId::proportional(11.0), FAINT);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.people.selected = if self.people.selected == Some(s.id) { None } else { Some(s.id) };
                }
            }
        });
        let _ = now;
    }

    fn people_detail(&mut self, ui: &mut Ui, store: &Arc<parking_lot::RwLock<PeopleStore>>, id: u32) {
        let pal = self.pal;
        let Some(s) = store.read().subject(id).cloned() else {
            self.people.selected = None;
            return;
        };
        let media = self.media.store.clone();
        let mut remove_face: Option<usize> = None;
        let mut delete = false;
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Name / relationship editing.
            let mut ed = self.people.edit.take().filter(|e| e.0 == id).unwrap_or_else(|| (id, s.name.clone(), s.relation.clone()));
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(if s.kind == Kind::Pet { "🐾" } else { "👤" }).size(18.0));
                ui.add(egui::TextEdit::singleline(&mut ed.1).desired_width(200.0).font(FontId::proportional(16.0)));
                relation_picker(ui, ("rel", id), &mut ed.2);
                ui.add(egui::TextEdit::singleline(&mut ed.2).desired_width(120.0).hint_text("or type one"));
                if theme::primary_button(ui, "Save", pal).clicked() {
                    let mut st = store.write();
                    if let Some(x) = st.subject_mut(id) {
                        x.name = ed.1.trim().to_string();
                        x.relation = ed.2.trim().to_string();
                    }
                    st.save();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::ghost_button(ui, "Remove").on_hover_text("Forget this name (photos are not touched)").clicked() {
                        delete = true;
                    }
                    let q = format!("person:{}", s.name.split_whitespace().next().unwrap_or(&s.name).to_lowercase());
                    if theme::ghost_button(ui, "🔍 Search").on_hover_text(format!("Open Search with {q}")).clicked() {
                        self.query = q;
                        self.set_page(ui, super::app::Page::Search);
                    }
                });
            });
            self.people.edit = Some(ed);
            ui.add_space(8.0);
            match s.kind {
                Kind::Person => {
                    let faces: Vec<(usize, String, [f32; 4], bool)> = {
                        let st = store.read();
                        st.faces_of(id).into_iter().map(|i| (i, st.faces[i].path.clone(), st.faces[i].rel, st.faces[i].manual)).collect()
                    };
                    ui.label(egui::RichText::new(format!("{} faces · click ✖ on any that isn't {}", faces.len(), s.name)).size(12.0).color(MUTED));
                    ui.horizontal_wrapped(|ui| {
                        for (i, path, rel, manual) in faces.iter().take(160) {
                            let (r, resp) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::click());
                            let tex = self.people.thumbs.get(ui.ctx(), path, *rel);
                            avatar(ui, r, tex, "…", if *manual { pal.accent2 } else { with_alpha(pal.accent2, 0.35) });
                            if resp.hovered() {
                                ui.painter().circle_filled(r.right_top() + vec2(-8.0, 8.0), 9.0, RED);
                                ui.painter().text(r.right_top() + vec2(-8.0, 8.0), Align2::CENTER_CENTER, "✖", FontId::proportional(10.0), Color32::WHITE);
                            }
                            if resp.on_hover_text(format!("{}\n{}", crate::ui::storage_view::shorten_home(path), if *manual { "confirmed" } else { "recognised automatically" })).clicked() {
                                remove_face = Some(*i);
                            }
                        }
                    });
                }
                Kind::Pet => self.pet_editor(ui, store, &s, &media),
            }
            // All photos.
            let photos = store.read().photos_of(id, &media.read());
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("{} photos with {}", fmt_count(photos.len() as u64), s.name)).size(13.0).strong());
            self.photo_strip(ui, &photos, 120, ("subj-photos", id));
        });
        if let Some(i) = remove_face {
            let mut st = store.write();
            if let Some(f) = st.faces.get_mut(i) {
                f.subject = None;
                f.manual = true; // "not this person": don't auto-assign it back
            }
            st.save();
        }
        if delete {
            let mut st = store.write();
            st.remove_subject(id);
            st.save();
            self.people.selected = None;
        }
    }

    fn pet_editor(&mut self, ui: &mut Ui, store: &Arc<parking_lot::RwLock<PeopleStore>>, s: &faces::Subject, media: &Arc<parking_lot::RwLock<crate::media::MediaStore>>) {
        let pal = self.pal;
        let mut st_changed = false;
        let mut thr = s.threshold;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{} example photos", s.examples.len())).size(12.5).color(MUTED));
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Match strictness").size(12.5).color(MUTED));
            if ui.add(egui::Slider::new(&mut thr, 0.78..=0.95).show_value(false)).changed() {
                st_changed = true;
            }
        });
        // Pick examples from photos labelled as animals.
        let cands: Vec<String> = {
            let m = media.read();
            let mut v = Vec::new();
            for l in ["dog", "cat", "pet", "bird", "horse"] {
                v.extend(m.paths_for_label(l));
            }
            v.dedup();
            v
        };
        let mut toggle_example: Option<String> = None;
        let mut confirm: Option<(String, bool)> = None;
        if s.examples.len() < 3 || ui.ctx().data(|d| d.get_temp::<bool>(Id::unique(("pick-ex", s.id))).unwrap_or(false)) {
            ui.label(egui::RichText::new(format!("Tap 3–10 photos of {} (from photos labelled dog, cat…):", s.name)).size(12.5).color(TEXT));
            egui::ScrollArea::horizontal().id_salt(("pet-cands", s.id)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for p in cands.iter().take(200) {
                        let (r, resp) = ui.allocate_exact_size(vec2(84.0, 84.0), Sense::click());
                        ui.painter().rect_filled(r, CornerRadius::same(8), SURFACE2);
                        if let Some((tex, size, _)) = self.thumbs.get(ui.ctx(), p) {
                            let m = size.x.min(size.y);
                            ui.painter().add(egui::epaint::RectShape::filled(r, 8, Color32::WHITE).with_texture(tex, Rect::from_center_size(pos2(0.5, 0.5), vec2(m / size.x, m / size.y))));
                        }
                        let on = s.examples.contains(p);
                        if on {
                            ui.painter().rect_stroke(r, CornerRadius::same(8), Stroke::new(3.0, AMBER), StrokeKind::Inside);
                            ui.painter().circle_filled(r.right_top() + vec2(-10.0, 10.0), 9.0, AMBER);
                            ui.painter().text(r.right_top() + vec2(-10.0, 10.0), Align2::CENTER_CENTER, "✔", FontId::proportional(11.0), Color32::BLACK);
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            toggle_example = Some(p.clone());
                        }
                    }
                });
            });
            if cands.is_empty() {
                ui.label(egui::RichText::new("No photos labelled dog/cat yet — run Analyze photos on the Media tab first.").size(12.0).color(AMBER));
            }
        }
        if s.examples.len() >= 3 {
            let picking = ui.ctx().data(|d| d.get_temp::<bool>(Id::unique(("pick-ex", s.id))).unwrap_or(false));
            if theme::ghost_button(ui, if picking { "Done picking examples" } else { "Edit examples" }).clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(Id::unique(("pick-ex", s.id)), !picking));
            }
            let sugg = faces::pet_suggestions(s, &media.read(), 24);
            if !sugg.is_empty() {
                ui.label(egui::RichText::new(format!("Is this {}? ✔ adds it, ✖ excludes it.", s.name)).size(12.5).color(TEXT));
                ui.horizontal_wrapped(|ui| {
                    for (p, sc) in &sugg {
                        let (r, resp) = ui.allocate_exact_size(vec2(96.0, 96.0), Sense::hover());
                        ui.painter().rect_filled(r, CornerRadius::same(8), SURFACE2);
                        if let Some((tex, size, _)) = self.thumbs.get(ui.ctx(), p) {
                            let m = size.x.min(size.y);
                            ui.painter().add(egui::epaint::RectShape::filled(r, 8, Color32::WHITE).with_texture(tex, Rect::from_center_size(pos2(0.5, 0.5), vec2(m / size.x, m / size.y))));
                        }
                        ui.painter().text(r.left_bottom() + vec2(6.0, -8.0), Align2::LEFT_BOTTOM, format!("{:.0}%", sc * 100.0), FontId::proportional(10.5), Color32::WHITE);
                        let yes = Rect::from_min_size(r.right_bottom() - vec2(48.0, 22.0), vec2(20.0, 18.0));
                        let no = Rect::from_min_size(r.right_bottom() - vec2(24.0, 22.0), vec2(20.0, 18.0));
                        let ry = ui.interact(yes, Id::unique(("pet-y", s.id, p)), Sense::click());
                        let rn = ui.interact(no, Id::unique(("pet-n", s.id, p)), Sense::click());
                        ui.painter().rect_filled(yes, CornerRadius::same(5), if ry.hovered() { GREEN } else { with_alpha(GREEN, 0.7) });
                        ui.painter().rect_filled(no, CornerRadius::same(5), if rn.hovered() { RED } else { with_alpha(RED, 0.7) });
                        ui.painter().text(yes.center(), Align2::CENTER_CENTER, "✔", FontId::proportional(11.0), Color32::WHITE);
                        ui.painter().text(no.center(), Align2::CENTER_CENTER, "✖", FontId::proportional(11.0), Color32::WHITE);
                        let _ = resp;
                        if ry.clicked() {
                            confirm = Some((p.clone(), true));
                        }
                        if rn.clicked() {
                            confirm = Some((p.clone(), false));
                        }
                    }
                });
            }
        }
        if toggle_example.is_some() || confirm.is_some() || st_changed {
            let mut st = store.write();
            if let Some(x) = st.subject_mut(s.id) {
                x.threshold = thr;
                if let Some(p) = toggle_example {
                    if let Some(i) = x.examples.iter().position(|e| *e == p) {
                        x.examples.remove(i);
                    } else {
                        x.examples.push(p);
                    }
                }
                if let Some((p, yes)) = confirm {
                    if yes {
                        x.confirmed.push(p);
                    } else {
                        x.rejected.push(p);
                    }
                }
            }
            st.save();
        }
        let _ = pal;
    }

    fn people_unnamed(&mut self, ui: &mut Ui, store: &Arc<parking_lot::RwLock<PeopleStore>>) {
        let pal = self.pal;
        let stamp = { let s = store.read(); s.faces.len() + s.faces.iter().filter(|f| f.subject.is_some()).count() * 7919 };
        if self.people.clusters.as_ref().is_none_or(|c| c.0 != stamp) {
            self.people.clusters = Some((stamp, store.read().clusters()));
        }
        let clusters = self.people.clusters.as_ref().map(|c| c.1.clone()).unwrap_or_default();
        let names: Vec<(u32, String)> = store.read().subjects.iter().filter(|s| s.kind == Kind::Person).map(|s| (s.id, s.name.clone())).collect();
        theme::section_title(ui, "Who are these?", pal);
        if clusters.is_empty() {
            ui.label(egui::RichText::new("No unnamed faces. Find faces in a folder to get started.").color(FAINT));
            return;
        }
        ui.label(egui::RichText::new("Each card is one person as far as Insight can tell. Type a name (or pick someone you already named) and their relationship — every matching face is named, now and in future scans.").size(12.0).color(MUTED));
        ui.add_space(6.0);
        let mut save: Option<(Vec<usize>, String, String)> = None;
        // Screenshot/test hook: INSIGHT_PEOPLE_NAME="0=Ana:Spouse" names group #0 once.
        if let Ok(spec) = std::env::var("INSIGHT_PEOPLE_NAME") {
            let done = Id::unique("people-name-hook");
            if !ui.ctx().data(|d| d.get_temp::<bool>(done).unwrap_or(false)) {
                ui.ctx().data_mut(|d| d.insert_temp(done, true));
                if let Some((i, rest)) = spec.split_once('=') {
                    if let (Ok(i), Some((n, r))) = (i.parse::<usize>(), rest.split_once(':')) {
                        if let Some(g) = clusters.get(i) {
                            save = Some((g.clone(), n.to_string(), r.to_string()));
                        }
                    }
                }
            }
        }
        for (ci, group) in clusters.iter().filter(|g| g.len() >= 2 || clusters.len() < 12).take(24).enumerate() {
            let key = group[0];
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let faces_v: Vec<(String, [f32; 4])> = { let s = store.read(); group.iter().take(8).map(|&i| (s.faces[i].path.clone(), s.faces[i].rel)).collect() };
                    for (path, rel) in &faces_v {
                        let (r, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
                        let tex = self.people.thumbs.get(ui.ctx(), path, *rel);
                        avatar(ui, r, tex, "…", with_alpha(pal.accent2, 0.4));
                    }
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(format!("{} photos", fmt_count(group.len() as u64))).size(12.5).color(MUTED));
                        let d = self.people.drafts.entry(key).or_insert_with(|| (String::new(), String::new()));
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut d.0).hint_text("Name").desired_width(150.0));
                            if !names.is_empty() {
                                egui::ComboBox::from_id_salt(("existing", ci)).selected_text("…or existing").width(110.0).show_ui(ui, |ui| {
                                    for (_, n) in &names {
                                        if ui.selectable_label(false, n).clicked() {
                                            d.0 = n.clone();
                                        }
                                    }
                                });
                            }
                            relation_picker(ui, ("draft-rel", key), &mut d.1);
                            if theme::primary_button(ui, "Save", pal).clicked() && !d.0.trim().is_empty() {
                                save = Some((group.clone(), d.0.clone(), d.1.clone()));
                            }
                        });
                    });
                });
            });
            ui.add_space(6.0);
        }
        if let Some((group, name, rel)) = save {
            let mut st = store.write();
            let id = match st.subjects.iter().find(|s| s.kind == Kind::Person && s.name.eq_ignore_ascii_case(name.trim())).map(|s| s.id) {
                Some(id) => {
                    if !rel.trim().is_empty() {
                        if let Some(s) = st.subject_mut(id) {
                            s.relation = rel.trim().to_string();
                        }
                    }
                    id
                }
                None => st.add_subject(&name, &rel, Kind::Person),
            };
            for i in &group {
                if let Some(f) = st.faces.get_mut(*i) {
                    f.subject = Some(id);
                    f.manual = true;
                }
            }
            let n = st.auto_assign();
            st.save();
            drop(st);
            self.people.drafts.remove(&group[0]);
            self.people.clusters = None;
            self.toast(ui, format!("Named {} · {} more faces recognised", name.trim(), n));
        }
    }

    /// A row of square photo thumbnails; click opens the file.
    pub fn photo_strip(&mut self, ui: &mut Ui, photos: &[String], max: usize, salt: impl std::hash::Hash + Copy) {
        ui.horizontal_wrapped(|ui| {
            for (k, p) in photos.iter().take(max).enumerate() {
                let (r, resp) = ui.allocate_exact_size(vec2(96.0, 96.0), Sense::click());
                ui.painter().rect_filled(r, CornerRadius::same(8), SURFACE2);
                if let Some((tex, size, fade)) = self.thumbs.get(ui.ctx(), p) {
                    let m = size.x.min(size.y);
                    ui.painter().add(egui::epaint::RectShape::filled(r, 8, with_alpha(Color32::WHITE, fade)).with_texture(tex, Rect::from_center_size(pos2(0.5, 0.5), vec2(m / size.x, m / size.y))));
                }
                let _ = (k, salt);
                if resp.on_hover_text(crate::ui::storage_view::shorten_home(p)).on_hover_cursor(egui::CursorIcon::PointingHand).double_clicked() {
                    Self::open_path(p);
                }
            }
            if photos.len() > max {
                ui.label(egui::RichText::new(format!("…and {} more", photos.len() - max)).color(FAINT));
            }
        });
        let _ = Path::new("");
    }
}
