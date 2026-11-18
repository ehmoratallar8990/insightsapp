//! Insight — instant, cross-platform file search with storage insights and
//! on-device media understanding. 100% Rust.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod engine;
mod filetype;
mod index;
mod media;
mod search;
mod storage;
mod ui;
mod walker;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // Headless helpers for scripting / benchmarking: `insight --cli <query>`.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--classify") {
        if let Err(e) = media::classify_cli(&args[2..]) {
            eprintln!("error: {e:#}");
        }
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("--cli") {
        cli(&args[2..]);
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Insight")
            .with_inner_size([1320.0, 840.0])
            .with_min_inner_size([860.0, 560.0])
            .with_app_id("insight")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap_or_default()),
        centered: true,
        ..Default::default()
    };
    eframe::run_native("Insight", options, Box::new(|cc| Ok(Box::new(ui::app::InsightApp::new(cc)))))
}

/// Minimal CLI: loads (or builds) the index and prints matches with timings.
fn cli(args: &[String]) {
    use std::time::Instant;
    let roots = index::default_roots();
    let t = Instant::now();
    let idx = match index::Index::load(&index::index_file()) {
        Ok(i) if i.roots == roots => {
            eprintln!("loaded {} entries in {:?}", i.live_count(), t.elapsed());
            i
        }
        _ => {
            eprintln!("no snapshot, scanning {roots:?} …");
            let i = index::Index::scan(&roots, &index::default_excludes(), &index::ScanProgress::default());
            eprintln!("scanned {} entries in {:?}", i.live_count(), t.elapsed());
            let _ = i.save(&index::index_file());
            i
        }
    };
    if args.first().map(String::as_str) == Some("--storage") {
        let t = Instant::now();
        let r = storage::analyze(&idx, 0);
        eprintln!("storage analysis in {:?}: {} suggestions, {} big files", t.elapsed(), r.suggestions.len(), r.largest_files.len());
        for s in &r.suggestions {
            println!("{:>10}  {}", storage::human(s.bytes), s.title);
        }
        return;
    }
    let q = search::Query::parse(&args.join(" "), None);
    let store = media::MediaStore::load();
    let out = search::run(&idx, &q, search::SortKey::Relevance, false, &store);
    for &id in out.ids.iter().take(30) {
        println!("{}", idx.path_string(id));
    }
    eprintln!("{} results in {:.2} ms", out.ids.len(), out.elapsed_us as f64 / 1000.0);
}
