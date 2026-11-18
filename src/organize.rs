//! Photo organizer: turns a messy folder of camera files into
//!
//! ```text
//! <dest>/2024/2024-07-14 Wedding/Canon EOS R8/RAW/IMG_0001.CR3
//!                                             /JPG/IMG_0001.JPG
//!                                /iPhone 15 Pro/HEIC/IMG_4410.HEIC
//! ```
//!
//! * Only files that are *clearly from a camera* (EXIF make/model + capture time, or
//!   a camera RAW format) are organized; screenshots, downloads and exports without
//!   camera metadata are reported and left alone.
//! * Photos are grouped into **events** by capture-time gaps, across all cameras.
//! * Non-destructive: originals are never moved or modified. Files are copied (APFS
//!   clones them instantly on the same volume), or optionally symlinked, and RAW files
//!   can be packed into one `RAW.zip` per camera. Every run writes a manifest so it
//!   can be undone.
//! * Everything runs on device; reading EXIF is pure Rust.

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDateTime};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Format {
    Raw,
    Jpeg,
    Heic,
    Other,
    Video,
}

impl Format {
    pub const ALL: [Format; 5] = [Format::Raw, Format::Jpeg, Format::Heic, Format::Other, Format::Video];
    pub fn folder(self) -> &'static str {
        match self {
            Format::Raw => "RAW",
            Format::Jpeg => "JPG",
            Format::Heic => "HEIC",
            Format::Other => "Other formats",
            Format::Video => "Videos",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Format::Raw => "RAW",
            Format::Jpeg => "JPEG",
            Format::Heic => "HEIC / HEIF",
            Format::Other => "PNG, TIFF & other",
            Format::Video => "Videos",
        }
    }
}

pub const RAW_EXTS: &[&str] = &["cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "dng", "pef", "srw", "x3f", "3fr", "iiq", "rwl", "erf", "mef", "mos", "kdc", "dcr"];

pub fn format_of(ext: &str) -> Option<Format> {
    let e = ext.to_ascii_lowercase();
    Some(match e.as_str() {
        x if RAW_EXTS.contains(&x) => Format::Raw,
        "jpg" | "jpeg" | "jpe" => Format::Jpeg,
        "heic" | "heif" | "hif" => Format::Heic,
        "png" | "tif" | "tiff" | "webp" | "avif" | "jxl" => Format::Other,
        "mp4" | "mov" | "m4v" | "mts" | "m2ts" | "avi" | "mkv" | "3gp" | "crm" => Format::Video,
        _ => return None,
    })
}

/// What the EXIF says about one file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub make: String,
    pub model: String,
    pub taken: Option<NaiveDateTime>,
}

#[derive(Clone, Debug)]
pub struct Photo {
    pub path: PathBuf,
    pub size: u64,
    pub taken: NaiveDateTime,
    pub camera: String,
    pub format: Format,
    /// Sharpness / exposure, shared by RAW+JPEG twins.
    pub quality: Option<Quality>,
    /// Goes to the event's `Rejects` folder.
    pub reject: bool,
    /// Short content label (from the on-device model) for the file-name suffix.
    pub label: Option<String>,
}

/// Cheap on-device image quality: is it sharp anywhere, is it exposed at all?
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quality {
    /// Laplacian variance of the sharpest regions (≈ "is anything in focus").
    pub sharpness: f32,
    /// Mean brightness 0..1.
    pub brightness: f32,
}

impl Quality {
    /// `sensitivity` 0..1: higher rejects more.
    pub fn is_bad(&self, sensitivity: f32) -> bool {
        let blur_limit = 8.0 + 90.0 * sensitivity.clamp(0.0, 1.0);
        self.sharpness < blur_limit || self.brightness < 0.035 || self.brightness > 0.97
    }
}

pub fn assess_image(img: &image::DynamicImage) -> Quality {
    let g = img.thumbnail(640, 640).to_luma8();
    let (w, h) = (g.width() as usize, g.height() as usize);
    let px = g.as_raw();
    let mean = px.iter().map(|&v| v as f64).sum::<f64>() / px.len().max(1) as f64 / 255.0;
    // Laplacian variance per tile; an image is sharp if *some* region is (shallow
    // depth of field is intentional, motion blur / missed focus is not).
    const T: usize = 4;
    let mut tiles = Vec::with_capacity(T * T);
    if w > 8 && h > 8 {
        for ty in 0..T {
            for tx in 0..T {
                let (x0, x1) = ((tx * w / T).max(1), ((tx + 1) * w / T).min(w - 1));
                let (y0, y1) = ((ty * h / T).max(1), ((ty + 1) * h / T).min(h - 1));
                let (mut sum, mut sq, mut n) = (0f64, 0f64, 0f64);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let c = px[y * w + x] as f64;
                        let l = 4.0 * c - px[y * w + x - 1] as f64 - px[y * w + x + 1] as f64 - px[(y - 1) * w + x] as f64 - px[(y + 1) * w + x] as f64;
                        sum += l;
                        sq += l * l;
                        n += 1.0;
                    }
                }
                if n > 0.0 {
                    tiles.push(sq / n - (sum / n).powi(2));
                }
            }
        }
    }
    tiles.sort_by(|a, b| b.total_cmp(a));
    let top = tiles.iter().take(2).sum::<f64>() / tiles.len().clamp(1, 2) as f64;
    Quality { sharpness: top as f32, brightness: mean as f32 }
}

/// Canon CR3 carries a ~1620 px JPEG preview (PRVW box); good enough to judge focus.
fn cr3_preview(path: &Path) -> Option<image::DynamicImage> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    f.by_ref().take(4 << 20).read_to_end(&mut head).ok()?;
    let p = memchr::memmem::find(&head, b"PRVW")?;
    let soi = p + memchr::memmem::find(&head[p..], &[0xFF, 0xD8, 0xFF])?;
    let eoi = soi + memchr::memmem::find(&head[soi..], &[0xFF, 0xD9])? + 2;
    image::load_from_memory(&head[soi..eoi]).ok()
}

/// JPEG decoded straight at 1/2–1/8 scale in the DCT domain: ~10× faster than a
/// full 24 MP decode and plenty for judging focus.
fn jpeg_small(path: &Path) -> Option<image::DynamicImage> {
    let f = std::fs::File::open(path).ok()?;
    let mut d = jpeg_decoder::Decoder::new(BufReader::new(f));
    d.scale(480, 480).ok()?;
    let px = d.decode().ok()?;
    let info = d.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    let luma: Vec<u8> = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => px,
        jpeg_decoder::PixelFormat::RGB24 => px.chunks_exact(3).map(|c| ((c[0] as u32 * 299 + c[1] as u32 * 587 + c[2] as u32 * 114) / 1000) as u8).collect(),
        _ => return None,
    };
    Some(image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(w, h, luma)?))
}

fn quality_of(path: &Path) -> Option<Quality> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let img = match ext.as_str() {
        "cr3" => cr3_preview(path)?,
        "jpg" | "jpeg" | "jpe" => jpeg_small(path).or_else(|| crate::media::decode::open(path, 640))?,
        _ => crate::media::decode::open(path, 640)?,
    };
    Some(assess_image(&img))
}

/// RAW+JPEG twins share a key: same folder, same file stem.
fn twin_key(p: &Path) -> (PathBuf, String) {
    (p.parent().map(Path::to_path_buf).unwrap_or_default(), p.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default())
}

/// Judge every photo (once per RAW+JPEG pair), in parallel.
pub fn assess(scan: &mut Scan, progress: &Progress) {
    let mut groups: HashMap<(PathBuf, String), Vec<usize>> = HashMap::new();
    for (i, p) in scan.photos.iter().enumerate() {
        groups.entry(twin_key(&p.path)).or_default().push(i);
    }
    let groups: Vec<Vec<usize>> = groups.into_values().collect();
    progress.total.store(groups.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let photos = &scan.photos;
    let results: Vec<(Vec<usize>, Option<Quality>)> = groups
        .into_par_iter()
        .map(|g| {
            if progress.cancel.load(Ordering::Relaxed) {
                return (g, None);
            }
            // Prefer the CR3's built-in 1.6 MP preview (≈15× less to decode than a
            // 24 MP JPEG), else any decodable twin.
            let pick = g
                .iter()
                .find(|&&i| photos[i].path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cr3")))
                .or_else(|| g.iter().find(|&&i| crate::filetype::is_decodable_image(&photos[i].path.to_string_lossy().to_lowercase())));
            let q = pick.and_then(|&i| quality_of(&photos[i].path)).or_else(|| {
                g.iter().find(|&&i| crate::filetype::is_decodable_image(&photos[i].path.to_string_lossy().to_lowercase())).and_then(|&i| quality_of(&photos[i].path))
            });
            progress.done.fetch_add(1, Ordering::Relaxed);
            (g, q)
        })
        .collect();
    for (g, q) in results {
        for i in g {
            scan.photos[i].quality = q;
        }
    }
}

/// (Re)apply the reject rule; `sensitivity` 0..1.
pub fn mark_rejects(scan: &mut Scan, sensitivity: f32) {
    for p in scan.photos.iter_mut() {
        p.reject = p.quality.is_some_and(|q| q.is_bad(sensitivity));
    }
}

/// Toggle a photo and its RAW/JPEG twin together.
pub fn set_reject(scan: &mut Scan, i: usize, reject: bool) {
    let key = twin_key(&scan.photos[i].path);
    for p in scan.photos.iter_mut() {
        if twin_key(&p.path) == key {
            p.reject = reject;
        }
    }
}

/// "Wedding Ana" → "wedding-ana" (max 3 words, 24 chars).
pub fn slug(s: &str) -> String {
    let words: Vec<String> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(3)
        .map(|w| w.to_lowercase())
        .collect();
    let mut out = words.join("-");
    out.truncate(out.char_indices().nth(24).map(|(i, _)| i).unwrap_or(out.len()));
    out.trim_end_matches('-').to_string()
}

#[derive(Default, Clone)]
pub struct Scan {
    pub photos: Vec<Photo>,
    /// Images without camera metadata (screenshots, downloads, exports…): left alone.
    pub others: Vec<(PathBuf, u64)>,
    pub unreadable: usize,
}

#[derive(Default)]
pub struct Progress {
    /// 0 = reading metadata, 1 = judging quality.
    pub phase: AtomicU64,
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

// ───────────────────────────── EXIF ─────────────────────────────

fn ascii(exif: &exif::Exif, tag: exif::Tag) -> String {
    exif.get_field(tag, exif::In::PRIMARY)
        .and_then(|f| match &f.value {
            exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).trim().trim_matches(char::from(0)).trim().to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn exif_date(exif: &exif::Exif) -> Option<NaiveDateTime> {
    for tag in [exif::Tag::DateTimeOriginal, exif::Tag::DateTimeDigitized, exif::Tag::DateTime] {
        let s = ascii(exif, tag);
        if let Ok(d) = NaiveDateTime::parse_from_str(s.get(..19).unwrap_or(&s), "%Y:%m:%d %H:%M:%S") {
            if d.year() > 1990 {
                return Some(d);
            }
        }
    }
    None
}

fn meta_from(exif: &exif::Exif, m: &mut Meta) {
    if m.make.is_empty() {
        m.make = ascii(exif, exif::Tag::Make);
    }
    if m.model.is_empty() {
        m.model = ascii(exif, exif::Tag::Model);
    }
    if m.taken.is_none() {
        m.taken = exif_date(exif);
    }
}

/// Read make / model / capture time. Only the file headers are read, so this is fast
/// even for 80 MB RAW files.
pub fn read_meta(path: &Path) -> Option<Meta> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mut f = std::fs::File::open(path).ok()?;
    let mut m = Meta::default();
    let reader = exif::Reader::new();
    match ext.as_str() {
        "mp4" | "mov" | "m4v" | "3gp" | "crm" => {
            m.taken = video_created(&mut f);
        }
        "mts" | "m2ts" | "avi" | "mkv" => {}
        "cr3" => {
            // Canon CR3 (ISO BMFF): TIFF blocks live in CMT1 (IFD0) and CMT2 (Exif IFD).
            let mut head = Vec::new();
            f.by_ref().take(2 << 20).read_to_end(&mut head).ok()?;
            for tag in [b"CMT1", b"CMT2"] {
                if let Some(p) = memchr::memmem::find(&head, tag) {
                    if p >= 4 {
                        let len = u32::from_be_bytes(head[p - 4..p].try_into().ok()?) as usize;
                        let end = (p - 4 + len).min(head.len());
                        if end > p + 4 {
                            if let Ok(e) = reader.read_raw(head[p + 4..end].to_vec()) {
                                meta_from(&e, &mut m);
                            }
                        }
                    }
                }
            }
        }
        "raf" => {
            // Fujifilm RAF: an embedded JPEG (with EXIF) at the offset stored at byte 84.
            let mut head = vec![0u8; 100];
            f.read_exact(&mut head).ok()?;
            let off = u32::from_be_bytes(head[84..88].try_into().ok()?) as u64;
            let len = u32::from_be_bytes(head[88..92].try_into().ok()?) as u64;
            f.seek(std::io::SeekFrom::Start(off)).ok()?;
            let mut jpg = Vec::new();
            f.take(len.min(4 << 20)).read_to_end(&mut jpg).ok()?;
            if let Ok(e) = reader.read_from_container(&mut std::io::Cursor::new(jpg)) {
                meta_from(&e, &mut m);
            }
        }
        x if RAW_EXTS.contains(&x) || x == "tif" || x == "tiff" => {
            // TIFF-based (CR2, NEF, ARW, DNG, ORF, RW2…): IFD0 + Exif IFD sit near the
            // start. Try a small window first, then a bigger one.
            for window in [1usize << 20, 8 << 20] {
                let mut head = Vec::new();
                f.seek(std::io::SeekFrom::Start(0)).ok()?;
                f.by_ref().take(window as u64).read_to_end(&mut head).ok()?;
                // Panasonic RW2 / Olympus ORF use non-standard TIFF magic; normalise it.
                if head.len() > 4 && (head[2] == 0x55 || head[2] == 0x52 || head[2] == 0x53) && head[..2] == *b"II" {
                    head[2] = 0x2a;
                    head[3] = 0;
                }
                if let Ok(e) = reader.read_raw(head.clone()) {
                    meta_from(&e, &mut m);
                    break;
                }
                if head.len() < window {
                    break;
                }
            }
        }
        _ => {
            let mut br = BufReader::new(f);
            if let Ok(e) = reader.read_from_container(&mut br) {
                meta_from(&e, &mut m);
            }
        }
    }
    Some(m)
}

/// Capture time of an MP4 / MOV from the `mvhd` box (seconds since 1904, UTC).
/// The box sits near the start or the end of the file; only those are read.
fn video_created(f: &mut std::fs::File) -> Option<NaiveDateTime> {
    let len = f.metadata().ok()?.len();
    let mut look = |off: u64| -> Option<NaiveDateTime> {
        let mut buf = Vec::new();
        f.seek(std::io::SeekFrom::Start(off)).ok()?;
        f.by_ref().take(4 << 20).read_to_end(&mut buf).ok()?;
        let p = memchr::memmem::find(&buf, b"mvhd")?;
        let v = *buf.get(p + 4)?;
        let secs = if v == 1 { u64::from_be_bytes(buf.get(p + 8..p + 16)?.try_into().ok()?) } else { u32::from_be_bytes(buf.get(p + 8..p + 12)?.try_into().ok()?) as u64 };
        let unix = secs.checked_sub(2_082_844_800)? as i64;
        let utc = chrono::DateTime::from_timestamp(unix, 0)?;
        let local: chrono::DateTime<chrono::Local> = utc.into();
        Some(local.naive_local()).filter(|d| d.year() > 2000)
    };
    look(0).or_else(|| look(len.saturating_sub(4 << 20)))
}

/// Human camera name: "Canon EOS R8", "iPhone 15 Pro", "Nikon Z 6", "Sony ILCE-7M4".
pub fn camera_name(make: &str, model: &str) -> String {
    let title = |s: &str| -> String {
        let s = s.trim();
        if s.chars().all(|c| !c.is_lowercase()) && s.len() > 3 {
            let mut c = s.chars();
            c.next().map(|f| f.to_string() + &c.as_str().to_lowercase()).unwrap_or_default()
        } else {
            s.to_string()
        }
    };
    let mut brand = make.trim().to_string();
    for junk in [" CORPORATION", " Corporation", " IMAGING CORP.", " Imaging Corp.", " COMPANY, LIMITED", " CO.,LTD.", " CO., LTD", " Co., Ltd.", ", Inc.", " Inc."] {
        brand = brand.replace(junk, "");
    }
    let brand = match brand.to_ascii_lowercase().as_str() {
        "olympus optical" | "om digital solutions" => "OM System".to_string(),
        "ricoh imaging" => "Ricoh".to_string(),
        _ => title(&brand),
    };
    // Canon writes Mark II bodies as "EOS M50m2" / "EOS 5D Mark IV" inconsistently.
    let model = {
        let m = model.trim();
        let b = m.as_bytes();
        let n = b.len();
        if n > 2 && b[n - 2] == b'm' && (b'2'..=b'5').contains(&b[n - 1]) && b[n - 3].is_ascii_digit() {
            let roman = ["II", "III", "IV", "V"][(b[n - 1] - b'2') as usize];
            format!("{} Mark {roman}", &m[..n - 2])
        } else {
            m.to_string()
        }
    };
    let model = model.as_str();
    if model.is_empty() {
        return if brand.is_empty() { "Unknown camera".into() } else { brand };
    }
    // Apple / Google / Samsung phones read better without the brand.
    if brand.eq_ignore_ascii_case("apple") || model.to_ascii_lowercase().starts_with(&brand.to_ascii_lowercase()) {
        let m = model.get(brand.len()..).filter(|_| model.to_ascii_lowercase().starts_with(&brand.to_ascii_lowercase())).unwrap_or(model).trim();
        let m = if m.is_empty() { model } else { m };
        return if brand.eq_ignore_ascii_case("apple") { m.to_string() } else { format!("{brand} {m}") };
    }
    if brand.is_empty() { model.to_string() } else { format!("{brand} {model}") }
}

/// Folder-name safe.
pub fn sanitize(s: &str) -> String {
    let out: String = s.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '-' } else { c }).collect();
    let out = out.trim().trim_matches('.').trim().to_string();
    if out.is_empty() { "Untitled".into() } else { out }
}

// ───────────────────────────── Scan ─────────────────────────────

fn list_images(dir: &Path, out: &mut Vec<PathBuf>, skip: Option<&Path>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let n = name.to_string_lossy();
        if n.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            // Never re-ingest the destination if it lives inside the source.
            if skip.is_some_and(|s| p == s) || n.ends_with(".app") || n.ends_with(".photoslibrary") {
                continue;
            }
            list_images(&p, out, skip);
        } else if ft.is_file() && p.extension().and_then(|x| format_of(&x.to_string_lossy())).is_some() {
            out.push(p);
        }
    }
}

/// Read camera metadata for every image under `src`; with `quality`, also judge
/// sharpness / exposure (a second, slower pass that decodes a small preview).
pub fn scan(src: &Path, dest: Option<&Path>, progress: &Progress, quality: bool) -> Scan {
    let mut files = Vec::new();
    list_images(src, &mut files, dest);
    progress.total.store(files.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let results: Vec<(PathBuf, u64, Option<Meta>)> = files
        .into_par_iter()
        .map(|p| {
            if progress.cancel.load(Ordering::Relaxed) {
                return (p, 0, None);
            }
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let m = read_meta(&p);
            progress.done.fetch_add(1, Ordering::Relaxed);
            (p, size, m)
        })
        .collect();
    let mut s = Scan::default();
    for (path, size, meta) in results {
        let fmt = path.extension().and_then(|x| format_of(&x.to_string_lossy())).unwrap_or(Format::Other);
        let Some(m) = meta else {
            s.unreadable += 1;
            continue;
        };
        let has_camera = !(m.make.is_empty() && m.model.is_empty());
        // RAW files are camera files even when the date has to come from the file.
        let taken = m.taken.or_else(|| {
            (fmt == Format::Raw).then(|| file_time(&path)).flatten()
        });
        let video = fmt == Format::Video;
        match (has_camera || fmt == Format::Raw || video, taken) {
            (true, Some(t)) => s.photos.push(Photo {
                path,
                size,
                taken: t,
                camera: if video && !has_camera { String::new() } else { camera_name(&m.make, &m.model) },
                format: fmt,
                quality: None,
                reject: false,
                label: None,
            }),
            (true, None) if video => s.others.push((path, size)),
            _ => s.others.push((path, size)),
        }
    }
    s.photos.sort_by(|a, b| a.taken.cmp(&b.taken).then(a.path.cmp(&b.path)));
    if quality && !progress.cancel.load(Ordering::Relaxed) {
        progress.phase.store(1, Ordering::Relaxed);
        assess(&mut s, progress);
    }
    s
}

fn file_time(p: &Path) -> Option<NaiveDateTime> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Local> = t.into();
    Some(dt.naive_local())
}

// ───────────────────────────── Plan ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Event {
    pub name: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub include: bool,
    /// Extra tags for this event (comma separated in the UI).
    pub tags: String,
    /// Multi-day events: one "Day N - YYYY-MM-DD" folder per day.
    pub by_day: bool,
    /// Indices into `Scan::photos`.
    pub photos: Vec<usize>,
}

impl Event {
    pub fn days(&self) -> i64 {
        (self.end.date() - self.start.date()).num_days() + 1
    }

    /// "Day 1 - 2025-07-14" (zero-padded when an event spans 10+ days).
    pub fn day_folder(&self, taken: NaiveDateTime) -> String {
        let n = (taken.date() - self.start.date()).num_days() + 1;
        let w = self.days().to_string().len();
        format!("Day {n:0w$} - {}", taken.format("%Y-%m-%d"))
    }

    pub fn folder(&self) -> String {
        format!("{} {}", self.start.format("%Y-%m-%d"), sanitize(&self.name)).trim().to_string()
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Start a new event after this many hours without photos.
    pub gap_hours: f32,
    pub formats: [bool; 5],
    pub split_formats: bool,
    pub zip_raw: bool,
    /// Move into the destination (copy → verify → remove original). Off = copy.
    pub move_files: bool,
    /// After a move, leave a symlink at the original path pointing to the new file.
    pub link_back: bool,
    /// Also bring images without camera metadata into `<dest>/Other images/`.
    pub include_others: bool,
    /// Tags applied to everything (comma separated).
    pub tags: String,
    /// `IMG_0001_wedding.CR3`: original name + a short readable suffix.
    pub suffix: bool,
    /// Blurry / badly exposed shots go to a `Rejects` folder.
    pub rejects: bool,
    /// 0..1, higher rejects more.
    pub reject_sensitivity: f32,
    /// New plans split multi-day events into day folders.
    pub split_days: bool,
    /// Renamed cameras: detected name → folder name.
    pub camera_names: BTreeMap<String, String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            gap_hours: 6.0,
            formats: [true; 5],
            split_formats: true,
            zip_raw: false,
            move_files: true,
            link_back: false,
            include_others: true,
            tags: String::new(),
            suffix: true,
            rejects: true,
            reject_sensitivity: 0.35,
            split_days: false,
            camera_names: BTreeMap::new(),
        }
    }
}

/// Folder names that say nothing about the event (card dumps, imports…).
fn generic_folder(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let dcim = n.len() == 8 && n[..3].chars().all(|c| c.is_ascii_digit()); // 100CANON, 101APPLE…
    dcim || n.is_empty()
        || ["dcim", "photos", "pictures", "images", "camera", "camera roll", "import", "imports", "downloads", "desktop", "raw", "jpg", "jpeg", "export", "exports", "untitled folder", "new folder", "card", "sd card", "eos_digital", "misc", "card dump", "dump", "backup", "backups", "old", "old stuff", "stuff", "new", "temp", "tmp", "unsorted", "to sort", "inbox", "phone", "iphone", "android", "mobile", "selects", "edited", "originals"]
            .contains(&n.as_str())
        || n.chars().filter(|c| c.is_ascii_digit()).count() * 2 > n.len()
}

/// Group photos into events by time gaps and suggest names.
pub fn plan(scan: &Scan, opts: &Options, src: &Path, labels: &dyn Fn(&Path) -> Option<String>) -> Vec<Event> {
    let gap = chrono::Duration::seconds((opts.gap_hours.max(0.25) * 3600.0) as i64);
    let mut events: Vec<Event> = Vec::new();
    for (i, p) in scan.photos.iter().enumerate() {
        match events.last_mut() {
            Some(e) if p.taken - e.end <= gap => {
                e.end = p.taken;
                e.photos.push(i);
            }
            _ => events.push(Event { name: String::new(), start: p.taken, end: p.taken, include: true, tags: String::new(), by_day: false, photos: vec![i] }),
        }
    }
    // A named folder is the photographer's own grouping: back-to-back events that
    // mostly come from the same named folder (a 3-day "Lisbon trip") become one.
    let dominant = |e: &Event| -> Option<String> {
        let mut c: HashMap<String, usize> = HashMap::new();
        for &i in &e.photos {
            if let Some(n) = parent_name(&scan.photos[i].path, src) {
                *c.entry(n).or_default() += 1;
            }
        }
        c.into_iter().max_by_key(|(_, n)| *n).filter(|(_, n)| n * 2 > e.photos.len()).map(|(k, _)| k)
    };
    let mut merged: Vec<Event> = Vec::new();
    for e in events {
        let join = merged.last().is_some_and(|prev| {
            (e.start - prev.end) <= chrono::Duration::days(3) && dominant(prev).is_some() && dominant(prev) == dominant(&e)
        });
        if join {
            let prev = merged.last_mut().unwrap();
            prev.end = prev.end.max(e.end);
            prev.photos.extend(e.photos);
        } else {
            merged.push(e);
        }
    }
    let mut events = merged;
    // Folder names that span most of the library (e.g. "Canon r8") aren't event names.
    let mut folder_events: HashMap<String, usize> = HashMap::new();
    for e in &events {
        let mut seen = std::collections::HashSet::new();
        for &i in &e.photos {
            if let Some(n) = parent_name(&scan.photos[i].path, src) {
                if seen.insert(n.clone()) {
                    *folder_events.entry(n).or_default() += 1;
                }
            }
        }
    }
    let n_events = events.len();
    for e in events.iter_mut() {
        e.name = suggest_name(e, scan, src, &folder_events, n_events, labels);
        e.by_day = opts.split_days && e.days() > 1;
    }
    events
}

fn parent_name(p: &Path, src: &Path) -> Option<String> {
    let parent = p.parent()?;
    if parent == src {
        return None;
    }
    // Walk up past generic folders (DCIM/100CANON → the folder above).
    let mut cur = parent;
    while cur.starts_with(src) && cur != src {
        let n = cur.file_name()?.to_string_lossy().to_string();
        if !generic_folder(&n) {
            return Some(n);
        }
        cur = cur.parent()?;
    }
    None
}

fn suggest_name(e: &Event, scan: &Scan, src: &Path, folder_events: &HashMap<String, usize>, n_events: usize, labels: &dyn Fn(&Path) -> Option<String>) -> String {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &i in &e.photos {
        if let Some(n) = parent_name(&scan.photos[i].path, src) {
            // Ignore folders that appear in most events (camera dumps, years…).
            if n_events < 3 || folder_events.get(&n).copied().unwrap_or(0) * 2 <= n_events {
                *counts.entry(n).or_default() += 1;
            }
        }
    }
    if let Some((n, c)) = counts.into_iter().max_by_key(|(_, c)| *c) {
        if c * 2 >= e.photos.len() {
            // Strip a leading date the folder may already carry ("2024-07-14 Wedding").
            let trimmed = n.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '_' || c == '.' || c == ' ');
            return if trimmed.is_empty() { n } else { trimmed.to_string() };
        }
    }
    // On-device CLIP labels, when the Media tab has analysed these photos.
    let mut lc: HashMap<String, usize> = HashMap::new();
    for &i in e.photos.iter().take(400) {
        if let Some(l) = labels(&scan.photos[i].path) {
            *lc.entry(l).or_default() += 1;
        }
    }
    if let Some((l, c)) = lc.into_iter().max_by_key(|(_, c)| *c) {
        if c * 3 >= e.photos.len().min(400) {
            let mut ch = l.chars();
            return ch.next().map(|f| f.to_uppercase().collect::<String>() + ch.as_str()).unwrap_or(l);
        }
    }
    let days = (e.end.date() - e.start.date()).num_days();
    if days >= 1 {
        format!("{} days", days + 1)
    } else {
        match chrono::Timelike::hour(&e.start) {
            5..=11 => "Morning".into(),
            12..=16 => "Afternoon".into(),
            17..=20 => "Evening".into(),
            _ => "Night".into(),
        }
    }
}

/// Merge event `i` into event `i - 1`.
pub fn merge_with_previous(events: &mut Vec<Event>, i: usize) {
    if i == 0 || i >= events.len() {
        return;
    }
    let e = events.remove(i);
    let prev = &mut events[i - 1];
    prev.end = prev.end.max(e.end);
    prev.photos.extend(e.photos);
}

// ───────────────────────────── Execute ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Op {
    pub src: PathBuf,
    /// For zipped RAWs: the zip file; the entry name is the source file name.
    pub dst: PathBuf,
    pub zip: bool,
    pub size: u64,
    pub tags: Vec<String>,
}

fn split_tags(s: &str) -> Vec<String> {
    s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
}

pub fn camera_folder(opts: &Options, camera: &str) -> String {
    sanitize(opts.camera_names.get(camera).map(String::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(camera))
}

/// Original name plus `_suffix` (content label, else the event name).
pub fn file_name(p: &Photo, suffix: bool, event_slug: &str) -> String {
    let orig = p.path.file_name().unwrap_or_default().to_string_lossy().to_string();
    if !suffix {
        return orig;
    }
    let tag = p.label.as_deref().map(slug).filter(|s| !s.is_empty()).unwrap_or_else(|| event_slug.to_string());
    if tag.is_empty() {
        return orig;
    }
    let stem = p.path.file_stem().unwrap_or_default().to_string_lossy();
    if stem.to_lowercase().ends_with(&format!("_{tag}")) {
        return orig;
    }
    match p.path.extension() {
        Some(ext) => format!("{stem}_{tag}.{}", ext.to_string_lossy()),
        None => format!("{stem}_{tag}"),
    }
}

/// Share content labels between RAW+JPEG twins (RAWs can't be analysed directly).
pub fn share_labels(scan: &mut Scan) {
    let mut by_key: HashMap<(PathBuf, String), String> = HashMap::new();
    for p in &scan.photos {
        if let Some(l) = &p.label {
            by_key.entry(twin_key(&p.path)).or_insert_with(|| l.clone());
        }
    }
    for p in scan.photos.iter_mut() {
        if p.label.is_none() {
            p.label = by_key.get(&twin_key(&p.path)).cloned();
        }
    }
}

/// The final list of operations for the preview's current state.
pub fn build_ops(scan: &Scan, events: &[Event], opts: &Options, dest: &Path) -> Vec<Op> {
    let global = split_tags(&opts.tags);
    let mut ops = Vec::new();
    for e in events.iter().filter(|e| e.include) {
        let mut tags = global.clone();
        tags.extend(split_tags(&e.tags));
        let base = dest.join(e.start.format("%Y").to_string()).join(e.folder());
        let event_slug = slug(&e.name);
        for &i in &e.photos {
            let p = &scan.photos[i];
            let fi = Format::ALL.iter().position(|f| *f == p.format).unwrap_or(3);
            if p.format == Format::Video && !opts.formats[4] {
                continue;
            }
            if !opts.formats[fi] {
                continue;
            }
            let day = if e.by_day { base.join(e.day_folder(p.taken)) } else { base.clone() };
            if p.format == Format::Video {
                let dir = if p.camera.is_empty() { day.join("Videos") } else { day.join(camera_folder(opts, &p.camera)).join("Videos") };
                ops.push(Op { src: p.path.clone(), dst: dir.join(file_name(p, opts.suffix, &event_slug)), zip: false, size: p.size, tags: tags.clone() });
                continue;
            }
            let mut dir = day.join(camera_folder(opts, &p.camera));
            let reject = opts.rejects && p.reject;
            let zip = opts.zip_raw && p.format == Format::Raw && !reject;
            if zip {
                ops.push(Op { src: p.path.clone(), dst: dir.join("RAW.zip"), zip: true, size: p.size, tags: tags.clone() });
                continue;
            }
            if reject {
                dir = dir.join("Rejects");
            } else if opts.split_formats {
                dir = dir.join(p.format.folder());
            }
            let name = file_name(p, opts.suffix, &event_slug);
            ops.push(Op { src: p.path.clone(), dst: dir.join(name), zip: false, size: p.size, tags: tags.clone() });
        }
    }
    if opts.include_others {
        for (p, size) in &scan.others {
            let name = p.file_name().unwrap_or_default();
            ops.push(Op { src: p.clone(), dst: dest.join("Other images").join(name), zip: false, size: *size, tags: global.clone() });
        }
    }
    // Two different files with the same name in one folder: suffix the later ones.
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    for op in ops.iter_mut().filter(|o| !o.zip) {
        let n = seen.entry(op.dst.clone()).or_default();
        *n += 1;
        if *n > 1 {
            let stem = op.dst.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let ext = op.dst.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
            op.dst = op.dst.with_file_name(format!("{stem} ({n}){ext}"));
        }
    }
    ops
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Copy → verify → remove the original. Everything ends up in the destination.
    Move,
    /// Verified copy; originals stay.
    Copy,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Files now in the destination (copied or moved).
    pub placed: usize,
    /// Originals removed after their copy was verified.
    pub moved: usize,
    /// Links left at the original locations.
    pub linked: usize,
    pub already_there: usize,
    pub failed: Vec<(String, String)>,
    pub bytes: u64,
    /// Source folders left empty by the move and removed.
    pub removed_dirs: usize,
    pub manifest: Option<PathBuf>,
}

/// One manifest line: enough to put everything back.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Entry {
    src: PathBuf,
    dst: PathBuf,
    /// Member name when `dst` is a RAW.zip.
    zip_name: Option<String>,
    /// xxh3-128 of the content (hex).
    hash: String,
    moved: bool,
    linked: bool,
    /// The identical file was already in the destination before this run.
    existed: bool,
    /// XMP sidecar this run wrote (tags on Windows).
    #[serde(default)]
    sidecar: Option<PathBuf>,
}

/// Content hash used to verify every copy before an original is removed.
pub fn hash_file(p: &Path) -> Result<u128> {
    let mut f = std::fs::File::open(p).with_context(|| format!("read {}", p.display()))?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.digest128())
}

fn same_volume(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| p.ancestors().find_map(|x| std::fs::metadata(x).ok()).map(|m| m.dev());
        dev(a).is_some() && dev(a) == dev(b)
    }
    #[cfg(not(unix))]
    {
        a.components().next() == b.components().next()
    }
}

/// True when moving from `src` to `dest` needs no extra space (same drive: copies are clones).
pub fn free_move(src: &Path, dest: &Path) -> bool {
    same_volume(src, dest) && cfg!(target_os = "macos")
}

fn make_link(target: &Path, at: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, at)
    }
    #[cfg(windows)]
    {
        // Symlinks need Developer Mode / admin; a hard link (same drive) works for
        // everyone and, like a symlink, costs no space.
        std::os::windows::fs::symlink_file(target, at).or_else(|_| std::fs::hard_link(target, at))
    }
}

/// Keywords as an XMP sidecar (`IMG_0412.CR3.xmp`): read by Lightroom, Capture One,
/// Bridge and darktable. Used for tags on Windows, which has no Finder-style tags.
/// Never overwrites an existing sidecar. Returns the path written.
pub fn write_xmp(file: &Path, tags: &[String]) -> Option<PathBuf> {
    if tags.is_empty() {
        return None;
    }
    let mut name = file.file_name()?.to_os_string();
    name.push(".xmp");
    let xmp = file.with_file_name(name);
    if xmp.exists() {
        return None;
    }
    let esc = |t: &str| t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let items: String = tags.iter().map(|t| format!("     <rdf:li>{}</rdf:li>\n", esc(t))).collect();
    let body = format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Insight\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n   <dc:subject>\n    <rdf:Bag>\n{items}    </rdf:Bag>\n   </dc:subject>\n  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n"
    );
    std::fs::write(&xmp, body).ok()?;
    Some(xmp)
}

/// Run the plan. In `Move` mode an original is removed only after its copy in the
/// destination has been read back and hashes identical.
pub fn execute(ops: &[Op], mode: Mode, link_back: bool, src_root: &Path, dest: &Path, progress: &Progress) -> Result<Report> {
    std::fs::create_dir_all(dest).with_context(|| format!("can't create {}", dest.display()))?;
    progress.total.store(ops.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    progress.bytes.store(0, Ordering::Relaxed);
    let entries = parking_lot::Mutex::new(Vec::<Entry>::new());
    let failed = parking_lot::Mutex::new(Vec::<(String, String)>::new());

    ops.par_iter().filter(|o| !o.zip).for_each(|op| {
        if progress.cancel.load(Ordering::Relaxed) {
            return;
        }
        match place(op, mode, link_back) {
            Ok(mut e) => {
                if !op.tags.is_empty() {
                    if cfg!(windows) {
                        e.sidecar = write_xmp(&op.dst, &op.tags);
                    } else {
                        let _ = set_tags(&op.dst, &op.tags, false);
                    }
                }
                entries.lock().push(e);
            }
            Err(e) => failed.lock().push((op.src.display().to_string(), format!("{e:#}"))),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
        progress.bytes.fetch_add(op.size, Ordering::Relaxed);
    });

    let mut zips: BTreeMap<PathBuf, Vec<&Op>> = BTreeMap::new();
    for op in ops.iter().filter(|o| o.zip) {
        zips.entry(op.dst.clone()).or_default().push(op);
    }
    zips.into_par_iter().for_each(|(zpath, members)| {
        if progress.cancel.load(Ordering::Relaxed) {
            return;
        }
        match zip_members(&zpath, &members, mode, progress) {
            Ok(es) => {
                if let Some(t) = members.first().map(|m| &m.tags).filter(|t| !t.is_empty()) {
                    let _ = set_tags(&zpath, t, false);
                }
                entries.lock().extend(es);
            }
            Err(e) => failed.lock().push((zpath.display().to_string(), format!("{e:#}"))),
        }
    });

    let entries = entries.into_inner();
    let mut report = Report {
        placed: entries.iter().filter(|e| !e.existed).count(),
        moved: entries.iter().filter(|e| e.moved).count(),
        linked: entries.iter().filter(|e| e.linked).count(),
        already_there: entries.iter().filter(|e| e.existed).count(),
        failed: failed.into_inner(),
        bytes: progress.bytes.load(Ordering::Relaxed),
        ..Default::default()
    };
    if mode == Mode::Move {
        report.removed_dirs = prune_empty(entries.iter().filter(|e| e.moved && !e.linked).map(|e| e.src.as_path()), src_root);
    }
    if !entries.is_empty() {
        // Unique even for runs within the same second: never overwrite an undo record.
        static RUN: AtomicU64 = AtomicU64::new(0);
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
        let mut m = dest.join(format!(".insight-organize-{stamp}.json"));
        while m.exists() {
            m = dest.join(format!(".insight-organize-{stamp}-{}.json", RUN.fetch_add(1, Ordering::Relaxed)));
        }
        let json = serde_json::json!({ "version": 2, "mode": if mode == Mode::Move { "move" } else { "copy" }, "entries": entries });
        std::fs::write(&m, serde_json::to_vec_pretty(&json)?)?;
        report.manifest = Some(m);
    }
    Ok(report)
}

/// Copy (clone) to a temp name, fsync, rename, read back and compare hashes; then,
/// for a move, remove the original (and optionally leave a link in its place).
fn place(op: &Op, mode: Mode, link_back: bool) -> Result<Entry> {
    let src_hash = hash_file(&op.src)?;
    let existed = match std::fs::symlink_metadata(&op.dst) {
        Ok(m) if m.is_file() && m.len() == op.size && hash_file(&op.dst)? == src_hash => true,
        Ok(_) => anyhow::bail!("a different file already exists at {}", op.dst.display()),
        Err(_) => false,
    };
    if !existed {
        if let Some(d) = op.dst.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = op.dst.with_extension(format!("{}.part", op.dst.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default()));
        // std::fs::copy clones on APFS: same-drive copies are instant and share storage.
        std::fs::copy(&op.src, &tmp).with_context(|| format!("copy {}", op.src.display()))?;
        {
            let f = std::fs::File::options().write(true).open(&tmp)?;
            if let Ok(t) = std::fs::metadata(&op.src).and_then(|m| m.modified()) {
                let _ = f.set_modified(t);
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &op.dst)?;
        if hash_file(&op.dst)? != src_hash {
            let _ = std::fs::remove_file(&op.dst);
            anyhow::bail!("verification failed for {} — original kept", op.src.display());
        }
    }
    let mut e = Entry { src: op.src.clone(), dst: op.dst.clone(), zip_name: None, hash: format!("{src_hash:032x}"), moved: false, linked: false, existed, sidecar: None };
    if mode == Mode::Move {
        std::fs::remove_file(&op.src).with_context(|| format!("remove original {}", op.src.display()))?;
        e.moved = true;
        if link_back {
            e.linked = make_link(&op.dst, &op.src).is_ok();
        }
    }
    Ok(e)
}

/// Pack RAWs into one archive, read every member back and verify it, then (move)
/// remove the originals. An existing archive is accepted only if it already holds
/// identical members.
fn zip_members(zpath: &Path, members: &[&Op], mode: Mode, progress: &Progress) -> Result<Vec<Entry>> {
    let mut names: HashMap<String, usize> = HashMap::new();
    let planned: Vec<(&Op, String, u128)> = members
        .iter()
        .map(|m| {
            let mut name = m.src.file_name().unwrap_or_default().to_string_lossy().to_string();
            let n = names.entry(name.clone()).or_default();
            *n += 1;
            if *n > 1 {
                name = format!("{name} ({n})");
            }
            Ok((*m, name, hash_file(&m.src)?))
        })
        .collect::<Result<_>>()?;
    let existed = zpath.exists();
    if !existed {
        if let Some(d) = zpath.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = zpath.with_extension("zip.part");
        let f = std::fs::File::create(&tmp)?;
        let mut z = zip::ZipWriter::new(std::io::BufWriter::new(f));
        // RAW data is already compressed; storing is as small and much faster.
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
        for (m, name, _) in &planned {
            if progress.cancel.load(Ordering::Relaxed) {
                drop(z);
                let _ = std::fs::remove_file(&tmp);
                anyhow::bail!("cancelled");
            }
            z.start_file(name.as_str(), opts)?;
            std::io::copy(&mut std::fs::File::open(&m.src)?, &mut z)?;
        }
        let w = z.finish()?;
        w.into_inner().map_err(|e| anyhow::anyhow!("{e}"))?.sync_all()?;
        std::fs::rename(&tmp, zpath)?;
    }
    // Verify every member against its original.
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zpath)?)?;
    for (_, name, h) in &planned {
        let mut entry = archive.by_name(name).with_context(|| format!("{name} missing from {}", zpath.display()))?;
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        if hasher.digest128() != *h {
            if !existed {
                let _ = std::fs::remove_file(zpath);
            }
            anyhow::bail!("{name} in {} doesn't match the original — originals kept", zpath.display());
        }
    }
    let mut out = Vec::new();
    for (m, name, h) in planned {
        let mut e = Entry { src: m.src.clone(), dst: zpath.to_path_buf(), zip_name: Some(name), hash: format!("{h:032x}"), moved: false, linked: false, existed, sidecar: None };
        if mode == Mode::Move {
            std::fs::remove_file(&m.src)?;
            e.moved = true;
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
        progress.bytes.fetch_add(m.size, Ordering::Relaxed);
        out.push(e);
    }
    Ok(out)
}

/// Remove folders the move left empty (ignoring Finder's .DS_Store), never `root` itself.
fn prune_empty<'a>(paths: impl Iterator<Item = &'a Path>, root: &Path) -> usize {
    let mut dirs = std::collections::BTreeSet::new();
    for p in paths {
        let mut d = p.parent();
        while let Some(x) = d {
            if x == root || !x.starts_with(root) {
                break;
            }
            dirs.insert(x.to_path_buf());
            d = x.parent();
        }
    }
    let mut n = 0;
    for d in dirs.iter().rev() {
        let names: Vec<String> = std::fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        if names.iter().all(|n| n == ".DS_Store") {
            let _ = std::fs::remove_file(d.join(".DS_Store"));
            if std::fs::remove_dir(d).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// Finder tags (macOS) / `user.xdg.tags` (Linux). Never touches a link's target.
pub fn set_tags(path: &Path, tags: &[String], is_link: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let arr = plist::Value::Array(tags.iter().map(|t| plist::Value::String(t.clone())).collect());
        let mut buf = Vec::new();
        arr.to_writer_binary(&mut buf)?;
        let p = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let name = c"com.apple.metadata:_kMDItemUserTags";
        let r = unsafe { libc::setxattr(p.as_ptr(), name.as_ptr(), buf.as_ptr() as *const _, buf.len(), 0, libc::XATTR_NOFOLLOW) };
        if r != 0 {
            anyhow::bail!("setxattr failed");
        }
        let _ = is_link;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        if is_link {
            return Ok(()); // user xattrs aren't allowed on symlinks
        }
        let v = tags.join(",");
        let p = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let name = c"user.xdg.tags";
        let r = unsafe { libc::setxattr(p.as_ptr(), name.as_ptr(), v.as_ptr() as *const _, v.len(), 0) };
        if r != 0 {
            anyhow::bail!("setxattr failed");
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = (path, tags, is_link);
        Ok(())
    }
}

/// Put things back exactly as they were: moved files return to their original paths
/// (verified), links are removed, copies this run created are deleted.
pub fn undo(manifest: &Path) -> Result<usize> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest)?)?;
    let dest = manifest.parent().unwrap_or(Path::new("/")).to_path_buf();
    let entries: Vec<Entry> = serde_json::from_value(v["entries"].clone()).unwrap_or_default();
    let mut restored = 0;
    let mut errors = Vec::new();
    let mut zips_to_remove = std::collections::BTreeSet::new();
    let mut dest_dirs = std::collections::BTreeSet::new();
    for e in &entries {
        if !e.dst.starts_with(&dest) {
            continue;
        }
        let r: Result<()> = (|| {
            if let Some(x) = &e.sidecar {
                let _ = std::fs::remove_file(x);
            }
            if e.linked {
                // A symlink, or (Windows fallback) a hard link to the same content.
                match std::fs::symlink_metadata(&e.src) {
                    Ok(m) if m.file_type().is_symlink() => std::fs::remove_file(&e.src)?,
                    Ok(m) if m.is_file() && format!("{:032x}", hash_file(&e.src)?) == e.hash => std::fs::remove_file(&e.src)?,
                    _ => {}
                }
            }
            if e.moved {
                if let Some(d) = e.src.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let tmp = e.src.with_extension("insight-restore");
                if let Some(name) = &e.zip_name {
                    let mut a = zip::ZipArchive::new(std::fs::File::open(&e.dst)?)?;
                    std::io::copy(&mut a.by_name(name)?, &mut std::fs::File::create(&tmp)?)?;
                } else if e.existed {
                    std::fs::copy(&e.dst, &tmp)?;
                } else if std::fs::rename(&e.dst, &e.src).is_ok() {
                    return Ok(()); // same drive: an atomic rename back
                } else {
                    std::fs::copy(&e.dst, &tmp)?;
                }
                if format!("{:032x}", hash_file(&tmp)?) != e.hash {
                    let _ = std::fs::remove_file(&tmp);
                    anyhow::bail!("restored copy of {} doesn't verify", e.src.display());
                }
                std::fs::rename(&tmp, &e.src)?;
                if e.zip_name.is_none() && !e.existed {
                    std::fs::remove_file(&e.dst)?;
                }
            } else if !e.existed && e.zip_name.is_none() {
                std::fs::remove_file(&e.dst)?;
            }
            Ok(())
        })();
        match r {
            Ok(()) => restored += 1,
            Err(err) => errors.push(format!("{err:#}")),
        }
        if e.zip_name.is_some() && !e.existed {
            zips_to_remove.insert(e.dst.clone());
        }
        let mut d = e.dst.parent();
        while let Some(x) = d {
            if x == dest {
                break;
            }
            dest_dirs.insert(x.to_path_buf());
            d = x.parent();
        }
    }
    if errors.is_empty() {
        for z in &zips_to_remove {
            let _ = std::fs::remove_file(z);
        }
    }
    for d in dest_dirs.iter().rev() {
        let _ = std::fs::remove_file(d.join(".DS_Store"));
        let _ = std::fs::remove_dir(d);
    }
    if !errors.is_empty() {
        anyhow::bail!("{} item(s) couldn't be restored (manifest kept): {}", errors.len(), errors[0]);
    }
    let _ = std::fs::remove_file(manifest);
    Ok(restored)
}

/// Most recent manifest in `dest`, for "Undo last run".
pub fn last_manifest(dest: &Path) -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dest)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".insight-organize-")))
        .collect();
    v.sort();
    v.pop()
}

pub fn year_of(e: &Event) -> i32 {
    e.start.year()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(camera_name("Canon", "Canon EOS R8"), "Canon EOS R8");
        assert_eq!(camera_name("Apple", "iPhone 15 Pro"), "iPhone 15 Pro");
        assert_eq!(camera_name("NIKON CORPORATION", "NIKON Z 6"), "Nikon Z 6");
        assert_eq!(camera_name("SONY", "ILCE-7M4"), "Sony ILCE-7M4");
        assert_eq!(camera_name("FUJIFILM", "X-T5"), "Fujifilm X-T5");
        assert_eq!(camera_name("Canon", "Canon EOS M50m2"), "Canon EOS M50 Mark II");
        assert_eq!(sanitize("a/b: c?"), "a-b- c-");
        assert!(generic_folder("100CANON"));
        assert!(generic_folder("DCIM"));
        assert!(generic_folder("20240714"));
        assert!(!generic_folder("Wedding Ana"));
    }

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn events_and_ops() {
        let src = Path::new("/src");
        let ph = |p: &str, t: &str, cam: &str, f: Format| Photo { path: PathBuf::from(p), size: 10, taken: dt(t), camera: cam.into(), format: f, quality: None, reject: false, label: None };
        let scan = Scan {
            photos: vec![
                ph("/src/Wedding/IMG_1.CR3", "2024-07-14 15:00", "Canon EOS R8", Format::Raw),
                ph("/src/Wedding/IMG_1.JPG", "2024-07-14 15:00", "Canon EOS R8", Format::Jpeg),
                ph("/src/Wedding/IMG_9.HEIC", "2024-07-14 18:00", "iPhone 15 Pro", Format::Heic),
                ph("/src/DCIM/100CANON/IMG_2.CR3", "2024-08-01 09:00", "Canon EOS R8", Format::Raw),
            ],
            others: vec![(PathBuf::from("/src/Screenshot.png"), 5)],
            unreadable: 0,
        };
        let opts = Options { suffix: false, ..Default::default() };
        let ev = plan(&scan, &opts, src, &|_| None);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].name, "Wedding");
        assert_eq!(ev[0].folder(), "2024-07-14 Wedding");
        assert_eq!(ev[1].name, "Morning");
        let ops = build_ops(&scan, &ev, &opts, Path::new("/dest"));
        let dsts: Vec<String> = ops.iter().map(|o| o.dst.display().to_string()).collect();
        assert!(dsts.contains(&"/dest/2024/2024-07-14 Wedding/Canon EOS R8/RAW/IMG_1.CR3".to_string()));
        assert!(dsts.contains(&"/dest/2024/2024-07-14 Wedding/Canon EOS R8/JPG/IMG_1.JPG".to_string()));
        assert!(dsts.contains(&"/dest/2024/2024-07-14 Wedding/iPhone 15 Pro/HEIC/IMG_9.HEIC".to_string()));
        assert_eq!(ops.len(), 5);
        assert!(dsts.contains(&"/dest/Other images/Screenshot.png".to_string()), "non-camera images go to Other images");
        let only_camera = build_ops(&scan, &ev, &Options { include_others: false, suffix: false, ..Default::default() }, Path::new("/dest"));
        assert_eq!(only_camera.len(), 4);
        // Suffixes (label shared with the RAW twin), rejects, day folders.
        let mut scan2 = scan.clone();
        scan2.photos[1].label = Some("bride and groom".into());
        share_labels(&mut scan2);
        scan2.photos[2].reject = true;
        let mut ev2 = ev.clone();
        ev2[0].by_day = true;
        let ops = build_ops(&scan2, &ev2, &Options::default(), Path::new("/dest"));
        let dsts: Vec<String> = ops.iter().map(|o| o.dst.display().to_string()).collect();
        assert!(dsts.contains(&"/dest/2024/2024-07-14 Wedding/Day 1 - 2024-07-14/Canon EOS R8/RAW/IMG_1_bride-and-groom.CR3".to_string()), "{dsts:?}");
        assert!(dsts.contains(&"/dest/2024/2024-07-14 Wedding/Day 1 - 2024-07-14/iPhone 15 Pro/Rejects/IMG_9_wedding.HEIC".to_string()), "{dsts:?}");
        // Zip RAWs, JPEG only off, renamed camera.
        let mut o2 = Options { zip_raw: true, suffix: false, include_others: false, ..Default::default() };
        o2.formats[1] = false;
        o2.camera_names.insert("Canon EOS R8".into(), "R8".into());
        let ops = build_ops(&scan, &ev, &o2, Path::new("/dest"));
        assert!(ops.iter().all(|o| o.dst.to_string_lossy().contains("/R8/") || o.dst.to_string_lossy().contains("iPhone")));
        assert_eq!(ops.iter().filter(|o| o.zip).count(), 2);
        assert!(ops.iter().all(|o| !o.dst.to_string_lossy().ends_with(".JPG")));
    }

    #[test]
    fn blur_detection() {
        // A sharp checkerboard vs. the same image heavily blurred, plus a black frame.
        let sharp = image::DynamicImage::ImageLuma8(image::ImageBuffer::from_fn(400, 300, |x, y| image::Luma([if (x / 8 + y / 8) % 2 == 0 { 30 } else { 220 }])));
        let blurred = sharp.blur(12.0);
        let black = image::DynamicImage::ImageLuma8(image::ImageBuffer::from_pixel(400, 300, image::Luma([2])));
        let (qs, qb, qk) = (assess_image(&sharp), assess_image(&blurred), assess_image(&black));
        assert!(!qs.is_bad(0.35), "{qs:?}");
        assert!(qb.is_bad(0.35), "{qb:?}");
        assert!(qk.is_bad(0.35), "{qk:?}");
        assert_eq!(slug("Wedding — Ana & José"), "wedding-ana-josé");
    }

    #[test]
    fn execute_copy_zip_undo() {
        let root = std::env::temp_dir().join(format!("insight-org-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.CR3"), vec![7u8; 1000]).unwrap();
        std::fs::write(src.join("a.JPG"), vec![8u8; 500]).unwrap();
        let dest = root.join("dest");
        let ops = vec![
            Op { src: src.join("a.CR3"), dst: dest.join("2024/x/Cam/RAW.zip"), zip: true, size: 1000, tags: vec!["client".into()] },
            Op { src: src.join("a.JPG"), dst: dest.join("2024/x/Cam/JPG/a.JPG"), zip: false, size: 500, tags: vec!["client".into()] },
        ];
        let p = Progress::default();
        // Copy mode: originals stay.
        let r = execute(&ops, Mode::Copy, false, &src, &dest, &p).unwrap();
        assert_eq!(r.placed, 2);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(std::fs::read(dest.join("2024/x/Cam/JPG/a.JPG")).unwrap(), vec![8u8; 500]);
        let mut z = zip::ZipArchive::new(std::fs::File::open(dest.join("2024/x/Cam/RAW.zip")).unwrap()).unwrap();
        assert_eq!(z.by_index(0).unwrap().size(), 1000);
        assert!(src.join("a.CR3").exists() && src.join("a.JPG").exists());
        // Re-run: nothing new, nothing overwritten.
        let r2 = execute(&ops, Mode::Copy, false, &src, &dest, &p).unwrap();
        assert_eq!((r2.placed, r2.already_there), (0, 2));
        undo(&r2.manifest.unwrap()).unwrap(); // undoing a no-op run keeps the files
        assert!(dest.join("2024/x/Cam/JPG/a.JPG").exists());
        undo(&r.manifest.unwrap()).unwrap();
        assert!(!dest.join("2024").exists());
        assert!(src.join("a.CR3").exists() && src.join("a.JPG").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn xmp_sidecar() {
        let d = std::env::temp_dir().join(format!("insight-xmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("IMG_1.CR3");
        std::fs::write(&f, b"x").unwrap();
        let x = write_xmp(&f, &["Wedding".into(), "Client: Ana & Luis".into()]).unwrap();
        assert_eq!(x, d.join("IMG_1.CR3.xmp"));
        let body = std::fs::read_to_string(&x).unwrap();
        assert!(body.contains("<rdf:li>Wedding</rdf:li>") && body.contains("Client: Ana &amp; Luis"));
        assert!(write_xmp(&f, &["Other".into()]).is_none(), "existing sidecars are never overwritten");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn move_verify_link_undo() {
        let root = std::env::temp_dir().join(format!("insight-mv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("card");
        std::fs::create_dir_all(src.join("DCIM/100CANON")).unwrap();
        std::fs::write(src.join("DCIM/100CANON/b.CR3"), vec![3u8; 4000]).unwrap();
        std::fs::write(src.join("DCIM/100CANON/b.JPG"), vec![4u8; 2000]).unwrap();
        std::fs::write(src.join("DCIM/100CANON/.DS_Store"), b"x").unwrap();
        let dest = root.join("Organized");
        let ops = vec![
            Op { src: src.join("DCIM/100CANON/b.CR3"), dst: dest.join("2025/e/Cam/RAW.zip"), zip: true, size: 4000, tags: vec![] },
            Op { src: src.join("DCIM/100CANON/b.JPG"), dst: dest.join("2025/e/Cam/JPG/b_x.JPG"), zip: false, size: 2000, tags: vec![] },
        ];
        let p = Progress::default();
        let r = execute(&ops, Mode::Move, true, &src, &dest, &p).unwrap();
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!((r.placed, r.moved), (2, 2));
        // The JPG now lives in the destination; a link at the old path points to it.
        let old = src.join("DCIM/100CANON/b.JPG");
        assert!(std::fs::symlink_metadata(&old).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&old).unwrap(), dest.join("2025/e/Cam/JPG/b_x.JPG"));
        assert_eq!(std::fs::read(&old).unwrap(), vec![4u8; 2000]);
        // The zipped RAW has no link (it lives inside the archive) and its original is gone.
        assert!(!src.join("DCIM/100CANON/b.CR3").exists());
        // Undo: originals come back byte-identical, destination is cleaned up.
        undo(&r.manifest.unwrap()).unwrap();
        assert_eq!(std::fs::read(src.join("DCIM/100CANON/b.CR3")).unwrap(), vec![3u8; 4000]);
        assert!(std::fs::symlink_metadata(&old).unwrap().is_file());
        assert_eq!(std::fs::read(&old).unwrap(), vec![4u8; 2000]);
        assert!(!dest.join("2025").exists());
        // Move without links: emptied card folders are pruned (but not the card root).
        let r = execute(&ops, Mode::Move, false, &src, &dest, &p).unwrap();
        assert_eq!(r.moved, 2);
        assert!(!src.join("DCIM").exists(), "empty DCIM/100CANON removed");
        assert!(src.exists());
        undo(&r.manifest.unwrap()).unwrap();
        assert!(src.join("DCIM/100CANON/b.JPG").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
