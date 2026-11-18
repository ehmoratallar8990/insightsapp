//! Renders the app icon: `cargo run --release --example make_icon -- assets/`
use image::{Rgba, RgbaImage};

fn render(size: u32) -> RgbaImage {
    let ss = 4; // supersampling
    let n = size * ss;
    let (a, b) = ([124.0, 92.0, 255.0], [34.0, 211.0, 238.0]);
    let mut big = RgbaImage::new(n, n);
    let nf = n as f32;
    let margin = nf * 0.09;
    let radius = nf * 0.2;
    for y in 0..n {
        for x in 0..n {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded square (macOS-style grid margin).
            let (l, r) = (margin, nf - margin);
            let cx = px.clamp(l + radius, r - radius);
            let cy = py.clamp(l + radius, r - radius);
            let inside = (px - cx).powi(2) + (py - cy).powi(2) <= radius * radius && px >= l && px <= r && py >= l && py <= r;
            if !inside {
                continue;
            }
            let t = ((px - l) / (r - l) * 0.6 + (py - l) / (r - l) * 0.4).clamp(0.0, 1.0);
            let mut c = [0.0f32; 3];
            for i in 0..3 {
                c[i] = a[i] + (b[i] - a[i]) * t;
            }
            // Diamond mark.
            let (mx, my) = ((px - nf / 2.0).abs(), (py - nf / 2.0).abs());
            let d = mx / (nf * 0.22) + my / (nf * 0.28);
            if d <= 1.0 {
                let inner = mx / (nf * 0.10) + my / (nf * 0.125);
                c = if inner <= 1.0 { [235.0, 232.0, 255.0] } else { [255.0, 255.0, 255.0] };
            }
            big.put_pixel(x, y, Rgba([c[0] as u8, c[1] as u8, c[2] as u8, 255]));
        }
    }
    image::imageops::resize(&big, size, size, image::imageops::FilterType::Triangle)
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "assets".into());
    for s in [16, 32, 64, 128, 256, 512, 1024] {
        render(s).save(format!("{out}/icon-{s}.png")).unwrap();
    }
}
