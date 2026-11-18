//! People & pets, fully on device.
//!
//! * **Faces:** OpenCV Zoo's YuNet detector (MIT) finds faces + 5 landmarks; each
//!   face is aligned to the standard 112×112 template and SFace (Apache-2.0) turns
//!   it into a 128-number fingerprint. Same person ⇔ cosine similarity ≳ 0.36.
//!   Both ONNX models run in pure Rust via `tract`; downloaded once (~37 MB),
//!   verified by SHA-256.
//! * **Subjects:** you name a group of faces ("Jose", relationship "Dad"); new photos
//!   of him are recognised automatically.
//! * **Pets:** faces don't work for animals, so a pet is learned from photos you mark
//!   ("this is Mushu"); look-alikes are found with the CLIP embeddings the Media tab
//!   already computed.

use anyhow::{Context, Result, bail};
use image::{DynamicImage, GenericImageView, RgbImage};
use parking_lot::RwLock;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tract_onnx::prelude::*;

pub const DET_URL: &str = "https://huggingface.co/opencv/face_detection_yunet/resolve/main/face_detection_yunet_2023mar.onnx";
pub const REC_URL: &str = "https://huggingface.co/opencv/face_recognition_sface/resolve/main/face_recognition_sface_2021dec.onnx";
const DET_SHA: &str = "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4";
const REC_SHA: &str = "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79";

/// OpenCV's recommended cosine threshold for SFace ("same identity" above this).
pub const SAME_PERSON: f32 = 0.363;
/// Stricter threshold for automatically naming a face.
pub const AUTO_ASSIGN: f32 = 0.42;

pub fn model_dir() -> PathBuf {
    crate::index::data_dir().join("models").join("faces")
}

pub fn models_ready() -> bool {
    model_dir().join("yunet.onnx").exists() && model_dir().join("sface.onnx").exists()
}

/// Download both models (if missing) and verify their checksums.
#[cfg(feature = "download")]
pub fn download(mut progress: impl FnMut(u64, u64)) -> Result<()> {
    use sha2::Digest;
    use std::io::{Read, Write};
    let dir = model_dir();
    std::fs::create_dir_all(&dir)?;
    let total_all = 232_589u64 + 38_696_353;
    let mut done_all = 0u64;
    for (url, file, sha) in [(DET_URL, "yunet.onnx", DET_SHA), (REC_URL, "sface.onnx", REC_SHA)] {
        let dst = dir.join(file);
        if dst.exists() {
            done_all += std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
            continue;
        }
        let resp = ureq::get(url).call().with_context(|| format!("GET {url}"))?;
        let mut r = resp.into_body().into_reader();
        let part = dst.with_extension("part");
        let mut f = std::io::BufWriter::new(std::fs::File::create(&part)?);
        let mut h = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            f.write_all(&buf[..n])?;
            done_all += n as u64;
            progress(done_all, total_all);
        }
        f.flush()?;
        drop(f);
        let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != sha {
            let _ = std::fs::remove_file(&part);
            bail!("{file}: checksum mismatch — download discarded");
        }
        std::fs::rename(&part, &dst)?;
    }
    Ok(())
}

#[cfg(not(feature = "download"))]
pub fn download(_: impl FnMut(u64, u64)) -> Result<()> {
    bail!("built without network support: put yunet.onnx and sface.onnx in {}", model_dir().display())
}

type Plan = Arc<TypedSimplePlan>;

pub struct Models {
    det: Plan,
    rec: Plan,
}

#[derive(Clone, Copy, Debug)]
pub struct Detection {
    /// x, y, w, h in source-image pixels.
    pub bbox: [f32; 4],
    pub score: f32,
    /// right eye, left eye, nose, right mouth, left mouth.
    pub lm: [[f32; 2]; 5],
}

impl Models {
    pub fn load() -> Result<Models> {
        let dir = model_dir();
        let det = tract_onnx::onnx().model_for_path(dir.join("yunet.onnx"))?.into_optimized()?.into_runnable()?;
        let rec = tract_onnx::onnx().model_for_path(dir.join("sface.onnx"))?.into_optimized()?.into_runnable()?;
        Ok(Models { det, rec })
    }

    /// YuNet on a 640×640 letterbox; boxes mapped back to `img` coordinates.
    pub fn detect(&self, img: &RgbImage, min_score: f32) -> Result<Vec<Detection>> {
        const S: u32 = 640;
        let (w, h) = img.dimensions();
        let scale = S as f32 / w.max(h) as f32;
        let (nw, nh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
        let small = image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle);
        // NCHW, BGR, raw 0..255 (YuNet's training format); zero padding.
        let mut t = tract_ndarray::Array4::<f32>::zeros((1, 3, S as usize, S as usize));
        for (x, y, p) in small.enumerate_pixels() {
            let (x, y) = (x as usize, y as usize);
            t[[0, 0, y, x]] = p[2] as f32;
            t[[0, 1, y, x]] = p[1] as f32;
            t[[0, 2, y, x]] = p[0] as f32;
        }
        let out = self.det.run(tvec!(Tensor::from(t).into()))?;
        let v: Vec<Vec<f32>> = out.iter().map(|o| o.to_plain_array_view::<f32>().map(|a| a.iter().copied().collect()).unwrap_or_default()).collect();
        if v.len() != 12 {
            bail!("unexpected YuNet outputs");
        }
        let mut dets = Vec::new();
        for (si, stride) in [8usize, 16, 32].iter().enumerate() {
            let cols = S as usize / stride;
            let (cls, obj, bbox, kps) = (&v[si], &v[3 + si], &v[6 + si], &v[9 + si]);
            let n = cols * cols;
            if cls.len() < n || bbox.len() < n * 4 || kps.len() < n * 10 {
                bail!("YuNet output size mismatch");
            }
            for idx in 0..n {
                let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
                if score < min_score {
                    continue;
                }
                let (r, c) = ((idx / cols) as f32, (idx % cols) as f32);
                let s = *stride as f32;
                let cx = (c + bbox[idx * 4]) * s;
                let cy = (r + bbox[idx * 4 + 1]) * s;
                let bw = bbox[idx * 4 + 2].exp() * s;
                let bh = bbox[idx * 4 + 3].exp() * s;
                let mut lm = [[0f32; 2]; 5];
                for (k, l) in lm.iter_mut().enumerate() {
                    *l = [((kps[idx * 10 + 2 * k] + c) * s) / scale, ((kps[idx * 10 + 2 * k + 1] + r) * s) / scale];
                }
                dets.push(Detection { bbox: [(cx - bw / 2.0) / scale, (cy - bh / 2.0) / scale, bw / scale, bh / scale], score, lm });
            }
        }
        Ok(nms(dets, 0.3))
    }

    /// 128-d L2-normalised SFace embedding of an aligned face.
    pub fn embed(&self, img: &RgbImage, d: &Detection) -> Result<[f32; 128]> {
        let face = align(img, &d.lm);
        // SFace takes RGB (OpenCV swaps its BGR input), raw 0..255.
        let mut t = tract_ndarray::Array4::<f32>::zeros((1, 3, 112, 112));
        for (x, y, p) in face.enumerate_pixels() {
            for ch in 0..3 {
                t[[0, ch, y as usize, x as usize]] = p[ch] as f32;
            }
        }
        let out = self.rec.run(tvec!(Tensor::from(t).into()))?;
        let a = out[0].to_plain_array_view::<f32>()?;
        let mut e = [0f32; 128];
        for (i, v) in a.iter().take(128).enumerate() {
            e[i] = *v;
        }
        let n = e.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        e.iter_mut().for_each(|x| *x /= n);
        Ok(e)
    }
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let x1 = a[0].max(b[0]);
    let y1 = a[1].max(b[1]);
    let x2 = (a[0] + a[2]).min(b[0] + b[2]);
    let y2 = (a[1] + a[3]).min(b[1] + b[3]);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    inter / (a[2] * a[3] + b[2] * b[3] - inter).max(1e-6)
}

fn nms(mut d: Vec<Detection>, thr: f32) -> Vec<Detection> {
    d.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keep: Vec<Detection> = Vec::new();
    for x in d {
        if keep.iter().all(|k| iou(&k.bbox, &x.bbox) < thr) {
            keep.push(x);
        }
    }
    keep
}

/// The ArcFace / SFace 112×112 landmark template.
const TEMPLATE: [[f32; 2]; 5] = [[38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366], [41.5493, 92.3655], [70.7299, 92.2041]];

/// Similarity transform (scale·rotation + translation) mapping `src` → `dst`
/// in the least-squares sense (Umeyama). Returns [a, b, tx, ty] for
/// x' = a·x − b·y + tx, y' = b·x + a·y + ty.
fn similarity(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> [f32; 4] {
    let n = 5.0;
    let (mut sx, mut sy, mut dx, mut dy) = (0.0, 0.0, 0.0, 0.0);
    for i in 0..5 {
        sx += src[i][0];
        sy += src[i][1];
        dx += dst[i][0];
        dy += dst[i][1];
    }
    let (sx, sy, dx, dy) = (sx / n, sy / n, dx / n, dy / n);
    let (mut num_a, mut num_b, mut den) = (0.0, 0.0, 0.0);
    for i in 0..5 {
        let (x, y) = (src[i][0] - sx, src[i][1] - sy);
        let (u, v) = (dst[i][0] - dx, dst[i][1] - dy);
        num_a += x * u + y * v;
        num_b += x * v - y * u;
        den += x * x + y * y;
    }
    let a = num_a / den.max(1e-6);
    let b = num_b / den.max(1e-6);
    [a, b, dx - (a * sx - b * sy), dy - (b * sx + a * sy)]
}

/// Warp the face to the 112×112 template (bilinear).
pub fn align(img: &RgbImage, lm: &[[f32; 2]; 5]) -> RgbImage {
    let [a, b, tx, ty] = similarity(lm, &TEMPLATE);
    // Inverse of [[a,-b],[b,a]] is 1/(a²+b²)·[[a,b],[-b,a]].
    let det = (a * a + b * b).max(1e-9);
    let (w, h) = img.dimensions();
    let mut out = RgbImage::new(112, 112);
    for v in 0..112u32 {
        for u in 0..112u32 {
            let (px, py) = (u as f32 - tx, v as f32 - ty);
            let x = (a * px + b * py) / det;
            let y = (-b * px + a * py) / det;
            out.put_pixel(u, v, bilinear(img, x, y, w, h));
        }
    }
    out
}

fn bilinear(img: &RgbImage, x: f32, y: f32, w: u32, h: u32) -> image::Rgb<u8> {
    if x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
        return image::Rgb([0, 0, 0]);
    }
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let p = |xx, yy| img.get_pixel(xx, yy).0;
    let (p00, p10, p01, p11) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
    let mut o = [0u8; 3];
    for c in 0..3 {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        o[c] = (top * (1.0 - fy) + bot * fy).round() as u8;
    }
    image::Rgb(o)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ───────────────────────────── Store ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Kind {
    #[default]
    Person,
    Pet,
}

/// Suggested relationships (free text is allowed too).
pub const RELATIONS: &[&str] = &[
    "Me", "Spouse", "Partner", "Son", "Daughter", "Child", "Mom", "Dad", "Brother", "Sister", "Grandma", "Grandpa", "Family", "Friend", "Colleague", "Pet",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subject {
    pub id: u32,
    pub name: String,
    pub relation: String,
    pub kind: Kind,
    /// Pets: photos you marked (CLIP examples). People: unused.
    #[serde(default)]
    pub examples: Vec<String>,
    /// Pets: photos you confirmed or rejected from suggestions.
    #[serde(default)]
    pub confirmed: Vec<String>,
    #[serde(default)]
    pub rejected: Vec<String>,
    /// Pets: CLIP similarity needed to count as a match.
    #[serde(default = "default_pet_threshold")]
    pub threshold: f32,
}

fn default_pet_threshold() -> f32 {
    0.86
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaceRec {
    pub path: String,
    /// Box relative to the (oriented) image: x, y, w, h in 0..1.
    pub rel: [f32; 4],
    pub score: f32,
    /// Quantised embedding (value·127).
    pub emb: Vec<i8>,
    /// Named subject (manual or automatic).
    pub subject: Option<u32>,
    /// Set by the user: never auto-reassign.
    #[serde(default)]
    pub manual: bool,
}

impl FaceRec {
    pub fn vec(&self) -> Vec<f32> {
        self.emb.iter().map(|&v| v as f32 / 127.0).collect()
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct PeopleStore {
    pub faces: Vec<FaceRec>,
    /// path → (size, mtime) already scanned (also files without faces).
    pub scanned: HashMap<String, (u64, u32)>,
    pub subjects: Vec<Subject>,
    pub next_id: u32,
}

fn store_file() -> PathBuf {
    crate::index::data_dir().join("people.json")
}

/// Process-wide handle so search (`person:`) and Organize (`_mushu`) can read it.
pub fn shared() -> &'static Arc<RwLock<PeopleStore>> {
    static S: std::sync::OnceLock<Arc<RwLock<PeopleStore>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Arc::new(RwLock::new(PeopleStore::load())))
}

impl PeopleStore {
    pub fn load() -> PeopleStore {
        std::fs::read(store_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let f = store_file();
        let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
        if let Ok(b) = serde_json::to_vec(self) {
            let tmp = f.with_extension("json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, f);
            }
        }
    }

    pub fn subject(&self, id: u32) -> Option<&Subject> {
        self.subjects.iter().find(|s| s.id == id)
    }

    pub fn subject_mut(&mut self, id: u32) -> Option<&mut Subject> {
        self.subjects.iter_mut().find(|s| s.id == id)
    }

    pub fn add_subject(&mut self, name: &str, relation: &str, kind: Kind) -> u32 {
        self.next_id = self.next_id.max(1);
        let id = self.next_id;
        self.next_id += 1;
        self.subjects.push(Subject {
            id,
            name: name.trim().to_string(),
            relation: relation.trim().to_string(),
            kind,
            examples: Vec::new(),
            confirmed: Vec::new(),
            rejected: Vec::new(),
            threshold: default_pet_threshold(),
        });
        id
    }

    pub fn remove_subject(&mut self, id: u32) {
        self.subjects.retain(|s| s.id != id);
        for f in self.faces.iter_mut().filter(|f| f.subject == Some(id)) {
            f.subject = None;
            f.manual = false;
        }
    }

    /// Mean embedding of each named person's confirmed faces.
    fn centroids(&self) -> Vec<(u32, Vec<f32>)> {
        let mut sums: HashMap<u32, (Vec<f32>, usize)> = HashMap::new();
        for f in self.faces.iter().filter(|f| f.manual) {
            if let Some(s) = f.subject {
                let e = sums.entry(s).or_insert_with(|| (vec![0.0; 128], 0));
                for (a, b) in e.0.iter_mut().zip(f.vec()) {
                    *a += b;
                }
                e.1 += 1;
            }
        }
        sums.into_iter()
            .map(|(id, (mut v, _))| {
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                v.iter_mut().for_each(|x| *x /= n);
                (id, v)
            })
            .collect()
    }

    /// Name every unconfirmed face that clearly matches a named person.
    pub fn auto_assign(&mut self) -> usize {
        let cents = self.centroids();
        // Manual faces per subject, to compare against individual examples too.
        let examples: Vec<(u32, Vec<f32>)> = self.faces.iter().filter(|f| f.manual).filter_map(|f| Some((f.subject?, f.vec()))).collect();
        let mut n = 0;
        for f in self.faces.iter_mut().filter(|f| !f.manual) {
            let v = f.vec();
            let best_c = cents.iter().map(|(id, c)| (*id, cosine(&v, c))).max_by(|a, b| a.1.total_cmp(&b.1));
            let best_e = examples.iter().map(|(id, e)| (*id, cosine(&v, e))).max_by(|a, b| a.1.total_cmp(&b.1));
            let pick = match (best_c, best_e) {
                (Some(c), _) if c.1 >= AUTO_ASSIGN => Some(c.0),
                (_, Some(e)) if e.1 >= AUTO_ASSIGN + 0.08 => Some(e.0),
                _ => None,
            };
            if f.subject != pick {
                n += 1;
            }
            f.subject = pick;
        }
        n
    }

    /// Group unnamed faces into clusters of probably-the-same person (greedy,
    /// biggest first), so you can name a whole group at once.
    pub fn clusters(&self) -> Vec<Vec<usize>> {
        let idx: Vec<usize> = (0..self.faces.len()).filter(|&i| self.faces[i].subject.is_none()).collect();
        let vecs: Vec<Vec<f32>> = idx.iter().map(|&i| self.faces[i].vec()).collect();
        let mut used = vec![false; idx.len()];
        let mut out: Vec<Vec<usize>> = Vec::new();
        // Seed order: most-connected faces first gives tighter clusters.
        let mut degree: Vec<(usize, usize)> = (0..idx.len())
            .into_par_iter()
            .map(|a| (a, (0..vecs.len()).filter(|&b| b != a && cosine(&vecs[a], &vecs[b]) >= 0.45).count()))
            .collect();
        degree.sort_by(|a, b| b.1.cmp(&a.1));
        for (a, _) in degree {
            if used[a] {
                continue;
            }
            used[a] = true;
            let mut group = vec![a];
            for b in 0..idx.len() {
                if !used[b] && cosine(&vecs[a], &vecs[b]) >= 0.45 {
                    used[b] = true;
                    group.push(b);
                }
            }
            out.push(group.into_iter().map(|g| idx[g]).collect());
        }
        out.sort_by(|a, b| b.len().cmp(&a.len()));
        out
    }

    /// Faces of a subject (people).
    pub fn faces_of(&self, id: u32) -> Vec<usize> {
        (0..self.faces.len()).filter(|&i| self.faces[i].subject == Some(id)).collect()
    }

    /// Every photo a subject is in. Pets use CLIP similarity to their examples.
    pub fn photos_of(&self, id: u32, media: &crate::media::MediaStore) -> Vec<String> {
        let Some(s) = self.subject(id) else { return Vec::new() };
        match s.kind {
            Kind::Person => {
                let mut v: Vec<String> = self.faces_of(id).into_iter().map(|i| self.faces[i].path.clone()).collect();
                v.sort();
                v.dedup();
                v
            }
            Kind::Pet => pet_matches(s, media).into_iter().map(|(p, _)| p).collect(),
        }
    }

    /// Names of the subjects in a photo (people by face, pets by CLIP).
    pub fn names_in(&self, path: &str, media: &crate::media::MediaStore) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in self.faces.iter().filter(|f| f.path == path) {
            if let Some(s) = f.subject.and_then(|id| self.subject(id)) {
                if !out.contains(&s.name) {
                    out.push(s.name.clone());
                }
            }
        }
        for s in self.subjects.iter().filter(|s| s.kind == Kind::Pet) {
            if pet_score(s, media, path).is_some_and(|sc| sc >= s.threshold) || s.confirmed.iter().any(|p| p == path) || s.examples.iter().any(|p| p == path) {
                if !s.rejected.iter().any(|p| p == path) && !out.contains(&s.name) {
                    out.push(s.name.clone());
                }
            }
        }
        out
    }

    /// Resolve `person:jose`, `who:dad`, `with:mushu` (name or relationship).
    pub fn find(&self, q: &str) -> Vec<u32> {
        let q = q.trim().to_lowercase();
        self.subjects
            .iter()
            .filter(|s| s.name.to_lowercase() == q || s.relation.to_lowercase() == q || s.name.to_lowercase().split_whitespace().next() == Some(q.as_str()))
            .map(|s| s.id)
            .collect()
    }
}

fn media_vec(media: &crate::media::MediaStore, path: &str) -> Option<Vec<f32>> {
    let &i = media.by_path.get(path)?;
    Some(media.records[i].emb.iter().map(|&v| v as f32 / 127.0).collect())
}

/// Best CLIP similarity of `path` to any of the pet's example photos.
pub fn pet_score(s: &Subject, media: &crate::media::MediaStore, path: &str) -> Option<f32> {
    let v = media_vec(media, path)?;
    s.examples.iter().chain(s.confirmed.iter()).filter_map(|e| media_vec(media, e)).map(|e| cosine(&v, &e)).fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.max(x))))
}

/// Pet photos: examples + confirmed + everything above the threshold (minus rejected), best first.
pub fn pet_matches(s: &Subject, media: &crate::media::MediaStore) -> Vec<(String, f32)> {
    let refs: Vec<Vec<f32>> = s.examples.iter().chain(s.confirmed.iter()).filter_map(|e| media_vec(media, e)).collect();
    let rejected: HashSet<&String> = s.rejected.iter().collect();
    let mut out: Vec<(String, f32)> = media
        .records
        .par_iter()
        .filter(|r| !rejected.contains(&r.path))
        .filter_map(|r| {
            let v: Vec<f32> = r.emb.iter().map(|&x| x as f32 / 127.0).collect();
            let best = refs.iter().map(|e| cosine(&v, e)).fold(f32::MIN, f32::max);
            let pinned = s.examples.contains(&r.path) || s.confirmed.contains(&r.path);
            (pinned || best >= s.threshold).then(|| (r.path.clone(), if pinned { 1.0 } else { best }))
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// Near-misses to show as "Is this Mushu?" suggestions.
pub fn pet_suggestions(s: &Subject, media: &crate::media::MediaStore, n: usize) -> Vec<(String, f32)> {
    let refs: Vec<Vec<f32>> = s.examples.iter().chain(s.confirmed.iter()).filter_map(|e| media_vec(media, e)).collect();
    if refs.is_empty() {
        return Vec::new();
    }
    let known: HashSet<&String> = s.examples.iter().chain(s.confirmed.iter()).chain(s.rejected.iter()).collect();
    let mut v: Vec<(String, f32)> = media
        .records
        .par_iter()
        .filter(|r| !known.contains(&r.path))
        .map(|r| {
            let e: Vec<f32> = r.emb.iter().map(|&x| x as f32 / 127.0).collect();
            (r.path.clone(), refs.iter().map(|x| cosine(&e, x)).fold(f32::MIN, f32::max))
        })
        .filter(|(_, sc)| *sc >= s.threshold - 0.08)
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v.truncate(n);
    v
}

// ───────────────────────────── Scanning ─────────────────────────────

#[derive(Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub faces: AtomicU64,
    pub cancel: AtomicBool,
}

/// Decode an image for face detection (EXIF-oriented; CR3 via its preview).
pub fn load_rgb(path: &Path) -> Option<RgbImage> {
    let img: DynamicImage = crate::organize::preview_image(path, 1600)?;
    Some(img.to_rgb8())
}

/// Detect + embed faces in every photo not yet scanned. Safe to run repeatedly.
pub fn scan(models: &Models, items: Vec<(String, u64, u32)>, store: &Arc<RwLock<PeopleStore>>, progress: &Progress) -> Result<usize> {
    let todo: Vec<(String, u64, u32)> = {
        let s = store.read();
        items.into_iter().filter(|(p, sz, mt)| s.scanned.get(p) != Some(&(*sz, *mt))).collect()
    };
    progress.total.store(todo.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let models = &*models;
    let found: Vec<(String, u64, u32, Vec<FaceRec>)> = todo
        .par_iter()
        .with_min_len(4)
        .filter_map(|(p, sz, mt)| {
            if progress.cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut faces = Vec::new();
            if let Some(img) = load_rgb(Path::new(p)) {
                let (w, h) = img.dimensions();
                if let Ok(dets) = models.detect(&img, 0.86) {
                    for d in dets.into_iter().filter(|d| d.bbox[2] >= 36.0 && d.bbox[3] >= 36.0) {
                        if let Ok(e) = models.embed(&img, &d) {
                            faces.push(FaceRec {
                                path: p.clone(),
                                rel: [d.bbox[0] / w as f32, d.bbox[1] / h as f32, d.bbox[2] / w as f32, d.bbox[3] / h as f32],
                                score: d.score,
                                emb: e.iter().map(|x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect(),
                                subject: None,
                                manual: false,
                            });
                        }
                    }
                }
            }
            progress.faces.fetch_add(faces.len() as u64, Ordering::Relaxed);
            progress.done.fetch_add(1, Ordering::Relaxed);
            Some((p.clone(), *sz, *mt, faces))
        })
        .collect();
    let mut s = store.write();
    let mut n = 0;
    for (p, sz, mt, faces) in found {
        s.faces.retain(|f| f.path != p || f.manual);
        n += faces.len();
        s.faces.extend(faces);
        s.scanned.insert(p, (sz, mt));
    }
    s.auto_assign();
    s.save();
    Ok(n)
}

/// Crop a face (with margin) for an avatar.
pub fn face_crop(path: &Path, rel: [f32; 4], side: u32) -> Option<image::RgbaImage> {
    let img = crate::organize::preview_image(path, 1600)?;
    let (w, h) = img.dimensions();
    let (fw, fh) = (rel[2] * w as f32, rel[3] * h as f32);
    let m = fw.max(fh) * 0.35;
    let size = fw.max(fh) + 2.0 * m;
    let cx = rel[0] * w as f32 + fw / 2.0;
    let cy = rel[1] * h as f32 + fh / 2.0;
    let x0 = (cx - size / 2.0).max(0.0) as u32;
    let y0 = (cy - size / 2.0).max(0.0) as u32;
    let s = (size as u32).min(w - x0).min(h - y0).max(1);
    Some(img.crop_imm(x0, y0, s, s).resize_exact(side, side, image::imageops::FilterType::Triangle).to_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_recovers_a_known_transform() {
        // Rotate 30°, scale 2, shift (10, -5): the solver must find exactly that.
        let (a, b) = (2.0 * 30f32.to_radians().cos(), 2.0 * 30f32.to_radians().sin());
        let src = [[1.0, 2.0], [5.0, 1.0], [3.0, 4.0], [0.0, 7.0], [6.0, 6.0]];
        let mut dst = [[0f32; 2]; 5];
        for i in 0..5 {
            dst[i] = [a * src[i][0] - b * src[i][1] + 10.0, b * src[i][0] + a * src[i][1] - 5.0];
        }
        let t = similarity(&src, &dst);
        assert!((t[0] - a).abs() < 1e-3 && (t[1] - b).abs() < 1e-3);
        assert!((t[2] - 10.0).abs() < 1e-2 && (t[3] + 5.0).abs() < 1e-2);
    }

    #[test]
    fn subjects_and_search() {
        let mut s = PeopleStore::default();
        let jose = s.add_subject("Jose", "Dad", Kind::Person);
        let cin = s.add_subject("Cinthia", "Spouse", Kind::Person);
        let mut e1 = vec![0i8; 128];
        e1[0] = 127;
        let mut e2 = vec![0i8; 128];
        e2[1] = 127;
        s.faces.push(FaceRec { path: "/a.jpg".into(), rel: [0.0; 4], score: 1.0, emb: e1.clone(), subject: Some(jose), manual: true });
        s.faces.push(FaceRec { path: "/b.jpg".into(), rel: [0.0; 4], score: 1.0, emb: e1.clone(), subject: None, manual: false });
        s.faces.push(FaceRec { path: "/c.jpg".into(), rel: [0.0; 4], score: 1.0, emb: e2, subject: None, manual: false });
        assert_eq!(s.auto_assign(), 1, "the same face is recognised as Jose");
        assert_eq!(s.faces[1].subject, Some(jose));
        assert_eq!(s.faces[2].subject, None, "a different face stays unnamed");
        assert_eq!(s.find("dad"), vec![jose]);
        assert_eq!(s.find("cinthia"), vec![cin]);
        assert_eq!(s.find("JOSE"), vec![jose]);
        let media = crate::media::MediaStore::default();
        assert_eq!(s.photos_of(jose, &media), vec!["/a.jpg".to_string(), "/b.jpg".to_string()]);
        assert_eq!(s.names_in("/b.jpg", &media), vec!["Jose".to_string()]);
    }
}
