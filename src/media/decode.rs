//! Image decoding shared by the thumbnailer and the classifier.

use image::{DynamicImage, RgbaImage, imageops::FilterType};
use std::path::Path;

/// Decode an image file. HEIC/HEIF are converted through the built-in macOS
/// `sips` tool (no pure-Rust HEVC decoder exists yet).
pub fn open(path: &Path, max_side: u32) -> Option<DynamicImage> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext == "heic" || ext == "heif" {
        return open_via_sips(path, max_side);
    }
    use image::ImageDecoder;
    let mut reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    reader.no_limits();
    let mut decoder = reader.into_decoder().ok()?;
    // Camera photos store rotation in EXIF; apply it so thumbnails and the model see them upright.
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    Some(if img.width().max(img.height()) > max_side * 2 {
        // Fast pre-shrink for very large photos.
        img.thumbnail(max_side, max_side)
    } else {
        img
    })
}

#[cfg(target_os = "macos")]
fn open_via_sips(path: &Path, max_side: u32) -> Option<DynamicImage> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let out = std::env::temp_dir().join(format!(
        "insight-sips-{}-{}.jpg",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let ok = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "jpeg", "-Z", &max_side.to_string()])
        .arg(path)
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let img = if ok { image::open(&out).ok() } else { None };
    let _ = std::fs::remove_file(&out);
    img
}

#[cfg(not(target_os = "macos"))]
fn open_via_sips(_path: &Path, _max_side: u32) -> Option<DynamicImage> {
    None
}

pub fn thumbnail(path: &Path, side: u32) -> Option<RgbaImage> {
    let img = open(path, side)?;
    Some(img.resize(side, side, FilterType::Triangle).to_rgba8())
}

/// CLIP preprocessing: resize shortest side to 224 (bicubic), centre-crop,
/// normalise with the CLIP mean/std. Returns CHW f32.
pub fn clip_pixels(img: &DynamicImage) -> Vec<f32> {
    const S: u32 = 224;
    const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
    const STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];
    let (w, h) = (img.width().max(1), img.height().max(1));
    let scale = S as f32 / w.min(h) as f32;
    let (nw, nh) = (((w as f32 * scale).round() as u32).max(S), ((h as f32 * scale).round() as u32).max(S));
    let resized = img.resize_exact(nw, nh, FilterType::CatmullRom).to_rgb8();
    let (x0, y0) = ((nw - S) / 2, (nh - S) / 2);
    let mut out = vec![0f32; 3 * (S * S) as usize];
    let plane = (S * S) as usize;
    for y in 0..S {
        for x in 0..S {
            let p = resized.get_pixel(x0 + x, y0 + y);
            let i = (y * S + x) as usize;
            for c in 0..3 {
                out[c * plane + i] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c];
            }
        }
    }
    out
}
