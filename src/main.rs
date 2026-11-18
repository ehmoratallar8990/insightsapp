//! Insight — instant, cross-platform file search with storage insights and
//! on-device media understanding. 100% Rust.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cleanup;
mod engine;
mod filetype;
mod health;
mod index;
mod media;
mod search;
mod storage;
mod ui;
mod update;
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
    if let Some(cmd) = args.get(1).filter(|a| ["--cleanup", "--optimize", "--apps", "--update-check", "--update-now", "--health"].contains(&a.as_str())) {
        tools_cli(cmd, &args[2..]);
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
    if args.first().map(String::as_str) == Some("--purge") {
        let t = Instant::now();
        let (sizes, counts) = idx.rollup();
        let found = cleanup::purge::find(&idx, &sizes, &counts);
        eprintln!("{} project artifacts in {:?}", found.len(), t.elapsed());
        for a in found.iter().take(30) {
            println!("{:>10}  {:<20} {} ({} days idle)", storage::human(a.bytes), a.kind, a.path, a.idle_secs / 86_400);
        }
        return;
    }
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

/// Headless access to the cleanup / health / update tools (read-only: nothing is deleted).
fn tools_cli(cmd: &str, _args: &[String]) {
    use std::time::Instant;
    let t = Instant::now();
    match cmd {
        "--cleanup" => {
            let cats = cleanup::clean::scan();
            let total: u64 = cats.iter().map(|c| c.bytes()).sum();
            for c in &cats {
                println!("{:>10}  {:<16} {} ({} items)", storage::human(c.bytes()), c.group.label(), c.title, c.items.len());
            }
            eprintln!("{} reclaimable in {} categories, scanned in {:?}", storage::human(total), cats.len(), t.elapsed());
        }
        "--optimize" => {
            for task in cleanup::optimize::tasks() {
                println!("{}  {}{}", task.icon, task.title, if task.admin { "  (admin)" } else { "" });
            }
            for s in cleanup::optimize::startup_items() {
                println!("  startup: {:<30} {}", s.name, s.detail);
            }
        }
        "--apps" => {
            let apps = cleanup::apps::list();
            eprintln!("{} apps in {:?}", apps.len(), t.elapsed());
            for a in apps.iter().take(15) {
                let l = cleanup::apps::leftovers(a);
                let lb: u64 = l.iter().map(|x| x.bytes).sum();
                println!("{:>10}  {:<28} {:<40} +{} leftovers ({})", storage::human(a.bytes), a.name, a.bundle_id, l.len(), storage::human(lb));
            }
        }
        "--update-check" => match update::latest() {
            Ok(Some(r)) => println!("update available: {} -> {} ({})", update::CURRENT, r.version, r.asset_url),
            Ok(None) => println!("up to date ({})", update::CURRENT),
            Err(e) => println!("check failed: {e:#}"),
        },
        "--update-now" => {
            // Check, download, verify and install over the running copy (no relaunch).
            let ctx = egui::Context::default();
            let u = update::Updater::new(ctx);
            u.check(true);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(200));
                match u.get() {
                    update::State::Checking | update::State::Downloading(..) => continue,
                    update::State::Ready(r, p) => {
                        println!("staged {} at {}", r.version, p.display());
                        match u.apply(false) {
                            Ok(()) => println!("installed {} over {}", r.version, update::install_target().map(|p| p.display().to_string()).unwrap_or_default()),
                            Err(e) => println!("install failed: {e:#}"),
                        }
                    }
                    other => println!("{other:?}"),
                }
                break;
            }
        }
        "--health" => {
            let ctx = egui::Context::default();
            let m = health::Monitor::start(ctx);
            m.set_active(true);
            std::thread::sleep(std::time::Duration::from_millis(3500));
            let s = m.get();
            let (score, grade, issues) = s.score();
            println!("{} · {} · {}", s.model, s.chip, s.os);
            println!("health {score} ({})", grade.label());
            for i in issues {
                println!("  -{:>2}  {}", i.penalty, i.text);
            }
            println!("cpu {:.0}%  temp {:?}  mem pressure {:.0}%  disk {} free of {}", s.cpu, s.cpu_temp, s.mem_pressure, storage::human(s.disk_available), storage::human(s.disk_total));
            if let Some(b) = &s.battery {
                println!("battery {:.0}% charging={} minutes={:?} health={:?} cycles={:?}", b.percent, b.charging, b.minutes, b.health, b.cycles);
            }
            println!("net down {:.0} B/s up {:.0} B/s", s.net_down, s.net_up);
            for v in &s.volumes {
                println!("volume {} at {}", v.name, v.mount);
            }
            for p in &s.peripherals {
                println!("bt {} ({}) {:?}", p.name, p.kind, p.battery);
            }
            for p in s.top_mem.iter().take(3) {
                println!("mem {} {}", p.name, storage::human(p.mem));
            }
        }
        _ => {}
    }
}
