//! On-device media understanding: CLIP embeddings, zero-shot labels and
//! natural-language image search. Everything runs locally.

pub mod clip;
pub mod decode;
pub mod labels;
pub mod tokenizer;

use crate::index::{Index, data_dir};
use crate::search::LabelSource;
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Minimum probability for a label to count as a match.
pub const LABEL_THRESHOLD: f32 = 0.12;
const PIPELINE_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaRecord {
    pub path: String,
    pub size: u64,
    pub mtime: u32,
    /// Quantised, L2-normalised CLIP embedding (value * 127).
    pub emb: Vec<i8>,
    /// (label index, probability * 255), best first.
    pub labels: Vec<(u16, u8)>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct MediaStore {
    pub records: Vec<MediaRecord>,
    pub label_version: u64,
    /// Bumped when preprocessing changes (e.g. EXIF orientation) so old embeddings are redone.
    #[serde(default)]
    pub pipeline: u32,
    #[serde(skip)]
    pub by_path: HashMap<String, usize>,
}

fn store_file() -> PathBuf {
    data_dir().join("media.bin")
}

impl MediaStore {
    pub fn load() -> MediaStore {
        let mut s: MediaStore = std::fs::read(store_file())
            .ok()
            .and_then(|b| bincode::deserialize(&b).ok())
            .unwrap_or_default();
        if s.pipeline != PIPELINE_VERSION {
            s.records.clear();
            s.pipeline = PIPELINE_VERSION;
        }
        s.reindex();
        s
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(data_dir());
        if let Ok(b) = bincode::serialize(self) {
            let tmp = store_file().with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, store_file());
            }
        }
    }

    fn reindex(&mut self) {
        self.by_path = self.records.iter().enumerate().map(|(i, r)| (r.path.clone(), i)).collect();
    }

    pub fn upsert(&mut self, r: MediaRecord) {
        match self.by_path.get(&r.path) {
            Some(&i) => self.records[i] = r,
            None => {
                self.by_path.insert(r.path.clone(), self.records.len());
                self.records.push(r);
            }
        }
    }

    pub fn is_current(&self, path: &str, size: u64, mtime: u32) -> bool {
        self.by_path.get(path).is_some_and(|&i| self.records[i].size == size && self.records[i].mtime == mtime)
    }

    pub fn labels_for(&self, path: &str) -> Vec<(&'static str, f32)> {
        let Some(&i) = self.by_path.get(path) else { return Vec::new() };
        self.records[i]
            .labels
            .iter()
            .filter_map(|&(l, p)| labels::LABELS.get(l as usize).map(|(n, _)| (*n, p as f32 / 255.0)))
            .collect()
    }

    /// (label, count) over the whole library, most frequent first.
    pub fn label_counts(&self) -> Vec<(&'static str, usize)> {
        let mut counts = vec![0usize; labels::LABELS.len()];
        for r in &self.records {
            for (k, &(l, p)) in r.labels.iter().enumerate() {
                if k == 0 || p as f32 / 255.0 >= LABEL_THRESHOLD {
                    if let Some(c) = counts.get_mut(l as usize) {
                        *c += 1;
                    }
                }
            }
        }
        let mut v: Vec<_> = counts
            .into_iter()
            .enumerate()
            .filter(|(_, c)| *c > 0)
            .map(|(i, c)| (labels::LABELS[i].0, c))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }

    pub fn paths_for_label(&self, label: &str) -> Vec<String> {
        let wanted: Vec<u16> = labels::LABELS
            .iter()
            .enumerate()
            .filter(|(_, (n, _))| n.contains(label))
            .map(|(i, _)| i as u16)
            .collect();
        let mut v: Vec<(f32, String)> = self
            .records
            .iter()
            .filter_map(|r| {
                r.labels
                    .iter()
                    .enumerate()
                    .find(|(k, (l, p))| wanted.contains(l) && (*k == 0 || *p as f32 / 255.0 >= LABEL_THRESHOLD))
                    .map(|(_, (_, p))| (*p as f32, r.path.clone()))
            })
            .collect();
        v.sort_by(|a, b| b.0.total_cmp(&a.0));
        v.into_iter().map(|(_, p)| p).collect()
    }
}

impl LabelSource for MediaStore {
    fn paths_with_label(&self, label: &str) -> HashSet<String> {
        self.paths_for_label(label).into_iter().map(|p| p.to_lowercase()).collect()
    }
}

/// Wrapper so the search engine can read labels through the shared lock.
pub struct SharedLabels(pub Arc<RwLock<MediaStore>>);
impl LabelSource for SharedLabels {
    fn paths_with_label(&self, label: &str) -> HashSet<String> {
        if let Some(who) = label.strip_prefix('@') {
            // person:/who:/with: → named people & pets (by name or relationship).
            let people = crate::faces::shared().read();
            let media = self.0.read();
            return people.find(who).into_iter().flat_map(|id| people.photos_of(id, &media)).map(|p| p.to_lowercase()).collect();
        }
        if let Some(c) = label.strip_prefix('#') {
            return crate::places::shared().read().in_country(c).into_iter().map(|p| p.to_lowercase()).collect();
        }
        self.0.read().paths_with_label(label)
    }
}

fn quantize(v: &[f32]) -> Vec<i8> {
    v.iter().map(|x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
}

fn softmax_top(sims: &[f32], k: usize) -> Vec<(u16, u8)> {
    // CLIP's learned logit scale is 100.
    let max = sims.iter().copied().fold(f32::MIN, f32::max);
    let exps: Vec<f32> = sims.iter().map(|s| ((s - max) * 100.0).exp()).collect();
    let sum: f32 = exps.iter().sum();
    let mut idx: Vec<usize> = (0..sims.len()).collect();
    idx.sort_by(|&a, &b| exps[b].total_cmp(&exps[a]));
    idx.into_iter()
        .take(k)
        .map(|i| (i as u16, ((exps[i] / sum) * 255.0).round() as u8))
        .filter(|(_, p)| *p >= 8)
        .collect()
}

fn label_record(emb: &[i8], label_embs: &[Vec<f32>]) -> Vec<(u16, u8)> {
    let sims: Vec<f32> = label_embs
        .iter()
        .map(|t| t.iter().zip(emb).map(|(a, b)| a * (*b as f32 / 127.0)).sum())
        .collect();
    let mut top = softmax_top(&sims, 5);
    if top.is_empty() {
        top = softmax_top(&sims, 1);
    }
    top
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Downloading { done: u64, total: u64 },
    Loading,
    Analyzing { done: usize, total: usize },
    Ready,
    Error(String),
}

pub struct MediaShared {
    pub phase: Phase,
    pub device: String,
    pub rate: f32,
    pub search: Option<(String, Vec<(String, f32)>)>,
    pub searching: bool,
}

enum Cmd {
    Analyze(Vec<(String, u64, u32)>),
    Search(String),
    /// Warm the model (when the Media page is opened) so the first search is instant.
    Prepare,
}

pub struct MediaEngine {
    tx: Sender<Cmd>,
    pub shared: Arc<Mutex<MediaShared>>,
    pub store: Arc<RwLock<MediaStore>>,
    pub stop: Arc<AtomicBool>,
    prepared: AtomicBool,
}

impl MediaEngine {
    pub fn start(ctx: egui::Context) -> Self {
        let (tx, rx) = unbounded();
        let shared = Arc::new(Mutex::new(MediaShared {
            phase: Phase::Idle,
            device: String::new(),
            rate: 0.0,
            search: None,
            searching: false,
        }));
        let store = Arc::new(RwLock::new(MediaStore::load()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (shared, store, stop) = (shared.clone(), store.clone(), stop.clone());
            std::thread::Builder::new()
                .name("media-engine".into())
                .spawn(move || worker(rx, shared, store, stop, ctx))
                .expect("spawn media engine");
        }
        MediaEngine { tx, shared, store, stop, prepared: AtomicBool::new(false) }
    }

    /// Load the model in the background (no-op if already loaded or not downloaded).
    pub fn prepare(&self) {
        if !self.prepared.swap(true, Ordering::Relaxed) {
            let _ = self.tx.send(Cmd::Prepare);
        }
    }
    pub fn analyze(&self, items: Vec<(String, u64, u32)>) {
        self.stop.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Analyze(items));
    }
    pub fn search(&self, q: String) {
        self.shared.lock().searching = true;
        let _ = self.tx.send(Cmd::Search(q));
    }
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Loaded {
    clip: clip::Clip,
    label_embs: Vec<Vec<f32>>,
}

fn ensure_loaded(
    loaded: &mut Option<Loaded>,
    shared: &Mutex<MediaShared>,
    store: &RwLock<MediaStore>,
    ctx: &egui::Context,
) -> bool {
    if loaded.is_some() {
        return true;
    }
    if !clip::is_downloaded() {
        shared.lock().phase = Phase::Downloading { done: 0, total: 0 };
        let mut last = Instant::now();
        let r = clip::download(|done, total| {
            if last.elapsed().as_millis() > 50 {
                shared.lock().phase = Phase::Downloading { done, total };
                ctx.request_repaint();
                last = Instant::now();
            }
        });
        if let Err(e) = r {
            shared.lock().phase = Phase::Error(format!("Model download failed: {e:#}"));
            ctx.request_repaint();
            return false;
        }
    }
    shared.lock().phase = Phase::Loading;
    ctx.request_repaint();
    let result = clip::Clip::load().and_then(|clip| {
        let label_embs = clip.embed_texts(&labels::prompts())?;
        Ok(Loaded { clip, label_embs })
    });
    match result {
        Ok(l) => {
            {
                let mut sh = shared.lock();
                sh.device = l.clip.device_name().to_string();
                sh.phase = Phase::Ready;
            }
            // Vocabulary changed since the library was analysed: re-label from embeddings.
            let ver = labels::version_hash();
            let mut st = store.write();
            if st.label_version != ver {
                let embs = &l.label_embs;
                st.records.par_iter_mut().for_each(|r| r.labels = label_record(&r.emb, embs));
                st.label_version = ver;
                st.save();
            }
            *loaded = Some(l);
            ctx.request_repaint();
            true
        }
        Err(e) => {
            shared.lock().phase = Phase::Error(format!("Could not load model: {e:#}"));
            ctx.request_repaint();
            false
        }
    }
}

fn worker(
    rx: Receiver<Cmd>,
    shared: Arc<Mutex<MediaShared>>,
    store: Arc<RwLock<MediaStore>>,
    stop: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let mut loaded: Option<Loaded> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Prepare => {
                if clip::is_downloaded() {
                    ensure_loaded(&mut loaded, &shared, &store, &ctx);
                }
            }
            Cmd::Search(q) => {
                if !ensure_loaded(&mut loaded, &shared, &store, &ctx) {
                    shared.lock().searching = false;
                    continue;
                }
                let l = loaded.as_ref().unwrap();
                let prompt = if q.split_whitespace().count() <= 3 { format!("a photo of {q}") } else { q.clone() };
                let results = match l.clip.embed_texts(&[prompt]) {
                    Ok(t) => {
                        let t = &t[0];
                        let st = store.read();
                        let mut scored: Vec<(f32, usize)> = st
                            .records
                            .par_iter()
                            .enumerate()
                            .map(|(i, r)| (t.iter().zip(&r.emb).map(|(a, b)| a * (*b as f32 / 127.0)).sum::<f32>(), i))
                            .collect();
                        scored.par_sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
                        scored.truncate(400);
                        scored.into_iter().map(|(s, i)| (st.records[i].path.clone(), s)).collect()
                    }
                    Err(e) => {
                        log::warn!("text embedding failed: {e}");
                        Vec::new()
                    }
                };
                let mut sh = shared.lock();
                sh.search = Some((q, results));
                sh.searching = false;
                ctx.request_repaint();
            }
            Cmd::Analyze(items) => {
                if !ensure_loaded(&mut loaded, &shared, &store, &ctx) {
                    continue;
                }
                let l = loaded.as_ref().unwrap();
                let todo: Vec<(String, u64, u32)> = {
                    let st = store.read();
                    items.into_iter().filter(|(p, s, m)| !st.is_current(p, *s, *m)).collect()
                };
                let total = todo.len();
                let t0 = Instant::now();
                let mut done = 0usize;
                shared.lock().phase = Phase::Analyzing { done, total };
                let batch = if matches!(l.clip.device, candle_core::Device::Cpu) { 16 } else { 32 };
                for (bi, chunk) in todo.chunks(batch).enumerate() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let decoded: Vec<(usize, Vec<f32>)> = chunk
                        .par_iter()
                        .enumerate()
                        .filter_map(|(i, (p, _, _))| decode::open(Path::new(p), 448).map(|img| (i, decode::clip_pixels(&img))))
                        .collect();
                    let pixels: Vec<Vec<f32>> = decoded.iter().map(|(_, p)| p.clone()).collect();
                    match l.clip.embed_images(&pixels) {
                        Ok(embs) => {
                            let mut st = store.write();
                            for ((i, _), e) in decoded.iter().zip(embs) {
                                let (p, s, m) = &chunk[*i];
                                let q = quantize(&e);
                                let labels = label_record(&q, &l.label_embs);
                                st.upsert(MediaRecord { path: p.clone(), size: *s, mtime: *m, emb: q, labels });
                            }
                            st.label_version = labels::version_hash();
                            if bi % 32 == 31 {
                                st.save();
                            }
                        }
                        Err(e) => log::warn!("embedding batch failed: {e}"),
                    }
                    done += chunk.len();
                    let mut sh = shared.lock();
                    sh.phase = Phase::Analyzing { done, total };
                    sh.rate = done as f32 / t0.elapsed().as_secs_f32().max(0.001);
                    drop(sh);
                    ctx.request_repaint();
                }
                store.read().save();
                shared.lock().phase = Phase::Ready;
                ctx.request_repaint();
            }
        }
    }
}

/// `insight --classify <images…>`: print top labels per image (downloads the model if needed).
pub fn classify_cli(paths: &[String]) -> anyhow::Result<()> {
    if !clip::is_downloaded() {
        eprintln!("downloading CLIP model…");
        let mut last = 0;
        clip::download(|d, t| {
            if d >> 26 != last {
                last = d >> 26;
                eprintln!("  {} / {} MB", d >> 20, t >> 20);
            }
        })?;
    }
    let t = Instant::now();
    let model = clip::Clip::load()?;
    let label_embs = model.embed_texts(&labels::prompts())?;
    eprintln!("model loaded on {} in {:?}", model.device_name(), t.elapsed());
    let t = Instant::now();
    let pixels: Vec<Vec<f32>> = paths
        .par_iter()
        .map(|p| decode::open(Path::new(p), 448).map(|i| decode::clip_pixels(&i)).ok_or_else(|| anyhow::anyhow!("cannot decode {p}")))
        .collect::<anyhow::Result<_>>()?;
    eprintln!("decoded {} images in {:?}", pixels.len(), t.elapsed());
    let t = Instant::now();
    let mut embs = Vec::new();
    for c in pixels.chunks(32) {
        embs.extend(model.embed_images(c)?);
    }
    eprintln!("embedded {} images in {:?} ({:.0} img/s)", embs.len(), t.elapsed(), embs.len() as f64 / t.elapsed().as_secs_f64());
    for (p, e) in paths.iter().zip(embs) {
        let q = quantize(&e);
        let top = label_record(&q, &label_embs);
        let s: Vec<String> = top.iter().map(|(l, pr)| format!("{} {:.0}%", labels::LABELS[*l as usize].0, *pr as f32 / 2.55)).collect();
        println!("{p}: {}", s.join(", "));
    }
    Ok(())
}

/// Image files worth analysing below `scope` (skips caches, app bundles, icons).
pub fn candidates(idx: &Index, scope: &str) -> Vec<(String, u64, u32)> {
    candidates_with(idx, scope, crate::filetype::is_decodable_image)
}

/// Like `candidates`, with a custom file-name filter (lower-case name).
pub fn candidates_with(idx: &Index, scope: &str, accept: fn(&str) -> bool) -> Vec<(String, u64, u32)> {
    let scope_l = scope.to_lowercase();
    let skip = ["/library/", "/.", "node_modules", ".app/", "/appdata/", "\\appdata\\", "/cache", "/caches/", "/thumbnails"];
    (0..idx.len() as u32)
        .into_par_iter()
        .filter_map(|id| {
            let e = &idx.entries[id as usize];
            if e.is_deleted() || e.kind() != crate::filetype::Kind::Image || e.size < 12 * 1024 || e.size > 120 << 20 {
                return None;
            }
            let lname = std::str::from_utf8(idx.lower_name(id)).ok()?;
            if !accept(lname) {
                return None;
            }
            let p = idx.path_string(id);
            let pl = p.to_lowercase();
            if !pl.starts_with(&scope_l) || skip.iter().any(|s| pl.contains(s)) {
                return None;
            }
            Some((p, e.size, e.mtime))
        })
        .collect()
}
