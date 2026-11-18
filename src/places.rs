//! Places: GPS from photo EXIF, an offline world map (Natural Earth, public domain)
//! and offline country lookup. Optional street-level OpenStreetMap tiles are fetched
//! by the map view only while zoomed in; photo locations never leave the computer.

use parking_lot::RwLock;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Countries as polygons (lon, lat), from the bundled Natural Earth 110m file.
pub struct World {
    pub countries: Vec<(String, Vec<Vec<[f32; 2]>>)>,
}

pub fn world() -> &'static World {
    static W: std::sync::OnceLock<World> = std::sync::OnceLock::new();
    W.get_or_init(|| {
        let v: serde_json::Value = serde_json::from_slice(include_bytes!("../assets/countries-110m.geojson")).unwrap_or_default();
        let mut countries = Vec::new();
        for f in v["features"].as_array().cloned().unwrap_or_default() {
            let name = f["properties"]["name"].as_str().unwrap_or("").to_string();
            let g = &f["geometry"];
            let polys: Vec<serde_json::Value> = match g["type"].as_str() {
                Some("Polygon") => vec![g["coordinates"].clone()],
                Some("MultiPolygon") => g["coordinates"].as_array().cloned().unwrap_or_default(),
                _ => Vec::new(),
            };
            let mut rings = Vec::new();
            for p in polys {
                // Outer ring only (holes are rare at this scale).
                if let Some(outer) = p.as_array().and_then(|r| r.first()).and_then(|r| r.as_array()) {
                    let ring: Vec<[f32; 2]> = outer.iter().filter_map(|c| Some([c[0].as_f64()? as f32, c[1].as_f64()? as f32])).collect();
                    if ring.len() > 2 {
                        rings.push(ring);
                    }
                }
            }
            countries.push((name, rings));
        }
        World { countries }
    })
}

fn in_ring(ring: &[[f32; 2]], x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi, xj, yj) = (ring[i][0], ring[i][1], ring[j][0], ring[j][1]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi + 1e-12) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Country containing (lat, lon), offline. Falls back to the nearest coastline
/// country for points just offshore (beaches, boats, coarse 110m borders).
pub fn country_of(lat: f64, lon: f64) -> Option<String> {
    let (x, y) = (lon as f32, lat as f32);
    let w = world();
    for (name, rings) in &w.countries {
        if rings.iter().any(|r| in_ring(r, x, y)) {
            return Some(name.clone());
        }
    }
    let mut best: Option<(f32, &String)> = None;
    for (name, rings) in &w.countries {
        for r in rings {
            for p in r.iter().step_by(2) {
                let d = (p[0] - x).powi(2) + (p[1] - y).powi(2);
                if best.is_none_or(|b| d < b.0) {
                    best = Some((d, name));
                }
            }
        }
    }
    best.filter(|b| b.0 < 1.0).map(|b| b.1.clone())
}

// ───────────────────────────── GPS ─────────────────────────────

fn rational_deg(f: &exif::Field) -> Option<f64> {
    match &f.value {
        exif::Value::Rational(v) if v.len() >= 3 => Some(v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0),
        exif::Value::Rational(v) if !v.is_empty() => Some(v[0].to_f64()),
        _ => None,
    }
}

fn ascii_first(f: &exif::Field) -> Option<u8> {
    match &f.value {
        exif::Value::Ascii(v) => v.first().and_then(|s| s.first()).copied(),
        _ => None,
    }
}

/// Read GPS from any EXIF block. GPS tags are matched by number so this also works
/// for Canon CR3's CMT4 box, where they sit in a plain IFD0.
fn gps_from(e: &exif::Exif) -> Option<(f64, f64)> {
    let (mut lat, mut lon, mut lat_ref, mut lon_ref) = (None, None, b'N', b'E');
    for f in e.fields() {
        if f.tag.context() != exif::Context::Gps {
            continue;
        }
        match f.tag.number() {
            1 => lat_ref = ascii_first(f).unwrap_or(b'N'),
            2 => lat = rational_deg(f).or(lat),
            3 => lon_ref = ascii_first(f).unwrap_or(b'E'),
            4 => lon = rational_deg(f).or(lon),
            _ => {}
        }
    }
    let (mut la, mut lo) = (lat?, lon?);
    if lat_ref == b'S' {
        la = -la;
    }
    if lon_ref == b'W' {
        lo = -lo;
    }
    // (0,0) and out-of-range values are "no fix".
    ((la.abs() > 1e-6 || lo.abs() > 1e-6) && la.abs() <= 90.0 && lo.abs() <= 180.0).then_some((la, lo))
}

/// Latitude/longitude of a photo, reading only its header.
pub fn read_gps(path: &Path) -> Option<(f64, f64)> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mut f = std::fs::File::open(path).ok()?;
    let reader = exif::Reader::new();
    match ext.as_str() {
        "cr3" => {
            let mut head = Vec::new();
            f.by_ref().take(2 << 20).read_to_end(&mut head).ok()?;
            let p = memchr::memmem::find(&head, b"CMT4")?;
            let len = u32::from_be_bytes(head.get(p - 4..p)?.try_into().ok()?) as usize;
            let end = (p - 4 + len).min(head.len());
            let e = reader.read_raw(head.get(p + 4..end)?.to_vec()).ok()?;
            // CMT4's IFD0 *is* the GPS IFD: tags 1–4 are the GPS ones here.
            let mut la = None;
            let mut lo = None;
            let (mut lr, mut or) = (b'N', b'E');
            for fl in e.fields() {
                match fl.tag.number() {
                    1 => lr = ascii_first(fl).unwrap_or(b'N'),
                    2 => la = rational_deg(fl),
                    3 => or = ascii_first(fl).unwrap_or(b'E'),
                    4 => lo = rational_deg(fl),
                    _ => {}
                }
            }
            let (mut a, mut o) = (la?, lo?);
            if lr == b'S' {
                a = -a;
            }
            if or == b'W' {
                o = -o;
            }
            ((a.abs() > 1e-6 || o.abs() > 1e-6) && a.abs() <= 90.0 && o.abs() <= 180.0).then_some((a, o))
        }
        x if crate::organize::RAW_EXTS.contains(&x) || x == "tif" || x == "tiff" => {
            let mut head = Vec::new();
            f.by_ref().take(4 << 20).read_to_end(&mut head).ok()?;
            let e = reader.read_raw(head).ok()?;
            gps_from(&e)
        }
        _ => {
            let e = if matches!(ext.as_str(), "heic" | "heif" | "hif" | "avif") {
                // Never the EXIF crate's HEIF parser (it can hang); see organize::heif_exif.
                reader.read_raw(crate::organize::heif_exif(path)?).ok()?
            } else {
                f.seek(std::io::SeekFrom::Start(0)).ok()?;
                reader.read_from_container(&mut BufReader::new(f)).ok()?
            };
            // Only trust GPS-context tags for normal photos.
            let mut lat = None;
            let mut lon = None;
            let (mut lr, mut or) = (b'N', b'E');
            for fl in e.fields().filter(|f| f.tag.context() == exif::Context::Gps) {
                match fl.tag.number() {
                    1 => lr = ascii_first(fl).unwrap_or(b'N'),
                    2 => lat = rational_deg(fl),
                    3 => or = ascii_first(fl).unwrap_or(b'E'),
                    4 => lon = rational_deg(fl),
                    _ => {}
                }
            }
            let (mut a, mut o) = (lat?, lon?);
            if lr == b'S' {
                a = -a;
            }
            if or == b'W' {
                o = -o;
            }
            ((a.abs() > 1e-6 || o.abs() > 1e-6) && a.abs() <= 90.0 && o.abs() <= 180.0).then_some((a, o))
        }
    }
}

// ───────────────────────────── Store ─────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Place {
    pub lat: f64,
    pub lon: f64,
    pub country: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct PlacesStore {
    /// path → location (photos with a GPS fix).
    pub places: HashMap<String, Place>,
    /// path → (size, mtime) already read (with or without GPS).
    pub scanned: HashMap<String, (u64, u32)>,
}

fn store_file() -> PathBuf {
    crate::index::data_dir().join("places.bin")
}

pub fn shared() -> &'static Arc<RwLock<PlacesStore>> {
    static S: std::sync::OnceLock<Arc<RwLock<PlacesStore>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Arc::new(RwLock::new(PlacesStore::load())))
}

impl PlacesStore {
    pub fn load() -> PlacesStore {
        std::fs::read(store_file()).ok().and_then(|b| bincode::deserialize(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) {
        let f = store_file();
        if let Ok(b) = bincode::serialize(self) {
            let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
            let tmp = f.with_extension("bin.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, f);
            }
        }
    }
    /// Photos in a country (case-insensitive, prefix ok: `country:port`).
    pub fn in_country(&self, q: &str) -> Vec<String> {
        let q = q.to_lowercase();
        self.places.iter().filter(|(_, p)| p.country.as_ref().is_some_and(|c| c.to_lowercase().starts_with(&q))).map(|(k, _)| k.clone()).collect()
    }
    pub fn countries(&self) -> Vec<(String, usize)> {
        let mut m: HashMap<String, usize> = HashMap::new();
        for p in self.places.values() {
            if let Some(c) = &p.country {
                *m.entry(c.clone()).or_default() += 1;
            }
        }
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }
}

#[derive(Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub found: AtomicU64,
    pub cancel: AtomicBool,
}

/// Read GPS for every not-yet-scanned image (headers only: thousands per second).
pub fn scan(items: Vec<(String, u64, u32)>, store: &Arc<RwLock<PlacesStore>>, progress: &Progress) -> usize {
    let todo: Vec<(String, u64, u32)> = {
        let s = store.read();
        items.into_iter().filter(|(p, sz, mt)| s.scanned.get(p) != Some(&(*sz, *mt))).collect()
    };
    progress.total.store(todo.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let found: Vec<(String, u64, u32, Option<Place>)> = todo
        .into_par_iter()
        .map(|(p, sz, mt)| {
            if progress.cancel.load(Ordering::Relaxed) {
                return (p, sz, mt, None);
            }
            let place = read_gps(Path::new(&p)).map(|(lat, lon)| Place { lat, lon, country: country_of(lat, lon) });
            if place.is_some() {
                progress.found.fetch_add(1, Ordering::Relaxed);
            }
            progress.done.fetch_add(1, Ordering::Relaxed);
            (p, sz, mt, place)
        })
        .collect();
    let mut s = store.write();
    let mut n = 0;
    for (p, sz, mt, place) in found {
        match place {
            Some(pl) => {
                s.places.insert(p.clone(), pl);
                n += 1;
            }
            None => {
                s.places.remove(&p);
            }
        }
        s.scanned.insert(p, (sz, mt));
    }
    s.save();
    n
}

/// Web-Mercator: (lat, lon) → (x, y) in 0..1.
pub fn mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = (lon + 180.0) / 360.0;
    let la = lat.clamp(-85.0511, 85.0511).to_radians();
    let y = (1.0 - (la.tan() + 1.0 / la.cos()).ln() / std::f64::consts::PI) / 2.0;
    (x, y)
}

pub fn unmercator(x: f64, y: f64) -> (f64, f64) {
    let lon = x * 360.0 - 180.0;
    let n = std::f64::consts::PI * (1.0 - 2.0 * y);
    let lat = n.sinh().atan().to_degrees();
    (lat, lon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countries_offline() {
        assert_eq!(country_of(38.72, -9.14).as_deref(), Some("Portugal")); // Lisbon
        assert_eq!(country_of(48.86, 2.35).as_deref(), Some("France")); // Paris
        assert_eq!(country_of(-33.87, 151.21).as_deref(), Some("Australia")); // Sydney
        assert_eq!(country_of(14.08, -87.21).as_deref(), Some("Honduras")); // Tegucigalpa
        assert_eq!(country_of(0.0, -140.0), None, "mid-Pacific");
    }

    #[test]
    fn mercator_roundtrip() {
        let (x, y) = mercator(38.72, -9.14);
        let (la, lo) = unmercator(x, y);
        assert!((la - 38.72).abs() < 1e-6 && (lo + 9.14).abs() < 1e-6);
    }
}
