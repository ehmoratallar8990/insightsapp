//! Background machinery: index lifecycle (load → watch → rescan → swap),
//! live file-system watching and the latest-wins search worker.

use crate::filetype::Kind;
use crate::index::{Index, ScanProgress, data_dir, index_file};
use crate::media::SharedLabels;
use crate::search::{self, Query, SortKey};
use crossbeam_channel::{Receiver, Sender, unbounded};
use notify::{RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexState {
    Loading,
    Scanning,
    Ready,
}

pub struct SearchRequest {
    pub seq: u64,
    pub text: String,
    pub kind: Option<Kind>,
    pub sort: SortKey,
    pub desc: bool,
}

pub struct SearchResponse {
    pub seq: u64,
    pub ids: Vec<u32>,
    pub elapsed_us: u64,
    pub highlight: Vec<String>,
    pub epoch: u64,
}

pub struct Engine {
    pub index: Arc<RwLock<Index>>,
    /// Bumps whenever the index content changes.
    pub epoch: Arc<AtomicU64>,
    /// Bumps when entry ids are invalidated (full swap / compaction).
    pub id_epoch: Arc<AtomicU64>,
    pub state: Arc<Mutex<IndexState>>,
    pub progress: Arc<ScanProgress>,
    pub scanning: Arc<AtomicBool>,
    pub last_scan_ms: Arc<AtomicU64>,
    pub roots: Arc<RwLock<Vec<String>>>,
    pub excludes: Arc<RwLock<Vec<String>>>,
    pub watch_error: Arc<Mutex<Option<String>>>,
    search_tx: Sender<SearchRequest>,
    pub search_rx: Receiver<SearchResponse>,
    events_during_scan: Arc<Mutex<Option<HashSet<PathBuf>>>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    watch_tx: Sender<PathBuf>,
    dirty: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Engine {
    pub fn start(
        ctx: egui::Context,
        roots: Vec<String>,
        excludes: Vec<String>,
        rescan_on_launch: bool,
        labels: SharedLabels,
    ) -> Arc<Engine> {
        let (search_tx, search_req_rx) = unbounded::<SearchRequest>();
        let (search_resp_tx, search_rx) = unbounded::<SearchResponse>();
        let (watch_tx, watch_rx) = unbounded::<PathBuf>();
        let eng = Arc::new(Engine {
            index: Arc::new(RwLock::new(Index::empty(roots.clone()))),
            epoch: Arc::new(AtomicU64::new(1)),
            id_epoch: Arc::new(AtomicU64::new(1)),
            state: Arc::new(Mutex::new(IndexState::Loading)),
            progress: Arc::new(ScanProgress::default()),
            scanning: Arc::new(AtomicBool::new(false)),
            last_scan_ms: Arc::new(AtomicU64::new(0)),
            roots: Arc::new(RwLock::new(roots)),
            excludes: Arc::new(RwLock::new(excludes)),
            watch_error: Arc::new(Mutex::new(None)),
            search_tx,
            search_rx,
            events_during_scan: Arc::new(Mutex::new(None)),
            watcher: Mutex::new(None),
            watch_tx,
            dirty: Arc::new(AtomicBool::new(false)),
            ctx,
        });

        // Search worker.
        {
            let e = eng.clone();
            std::thread::Builder::new()
                .name("search".into())
                .spawn(move || search_worker(e, search_req_rx, search_resp_tx, labels))
                .unwrap();
        }
        // Watch-event applier.
        {
            let e = eng.clone();
            std::thread::Builder::new().name("fs-events".into()).spawn(move || event_worker(e, watch_rx)).unwrap();
        }
        // Periodic persistence.
        {
            let e = eng.clone();
            std::thread::Builder::new()
                .name("autosave".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(120));
                    if e.dirty.swap(false, Ordering::Relaxed) && !e.scanning.load(Ordering::Relaxed) {
                        e.save();
                    }
                })
                .unwrap();
        }
        // Startup: load snapshot, start watching, then refresh in the background.
        {
            let e = eng.clone();
            std::thread::Builder::new()
                .name("startup".into())
                .spawn(move || {
                    let roots = e.roots.read().clone();
                    let loaded = Index::load(&index_file()).ok().filter(|i| i.roots == roots);
                    let have_snapshot = loaded.is_some();
                    if let Some(idx) = loaded {
                        e.last_scan_ms.store(idx.scan_ms, Ordering::Relaxed);
                        *e.index.write() = idx;
                        e.bump(true);
                        *e.state.lock() = IndexState::Ready;
                    }
                    e.start_watching();
                    if !have_snapshot || rescan_on_launch {
                        e.rescan();
                    }
                })
                .unwrap();
        }
        eng
    }

    fn bump(&self, ids_changed: bool) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
        if ids_changed {
            self.id_epoch.fetch_add(1, Ordering::Relaxed);
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.ctx.request_repaint();
    }

    pub fn search(&self, req: SearchRequest) {
        let _ = self.search_tx.send(req);
    }

    pub fn save(&self) {
        let idx = self.index.read();
        if idx.len() > 0 {
            if let Err(e) = idx.save(&index_file()) {
                log::warn!("saving index failed: {e:#}");
            }
        }
    }

    pub fn start_watching(self: &Arc<Self>) {
        let tx = self.watch_tx.clone();
        let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if ev.need_rescan() {
                    let _ = tx.send(PathBuf::from("\0rescan"));
                }
                for p in ev.paths {
                    let _ = tx.send(p);
                }
            }
        });
        let mut w = match w {
            Ok(w) => w,
            Err(e) => {
                *self.watch_error.lock() = Some(format!("Live updates unavailable: {e}"));
                return;
            }
        };
        let roots = self.roots.read().clone();
        for r in roots {
            // inotify needs one watch per directory; watching "/" would exhaust the
            // kernel limit, so on Linux we watch the home folder live and rely on
            // periodic rescans elsewhere.
            #[cfg(target_os = "linux")]
            let r = if r == "/" { dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or(r) } else { r };
            if let Err(e) = w.watch(std::path::Path::new(&r), RecursiveMode::Recursive) {
                *self.watch_error.lock() = Some(format!("Could not watch {r}: {e}"));
            }
        }
        *self.watcher.lock() = Some(w);
    }

    /// Full background rescan; swaps the index atomically when done.
    pub fn rescan(self: &Arc<Self>) {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return;
        }
        let e = self.clone();
        std::thread::Builder::new()
            .name("scanner".into())
            .spawn(move || {
                {
                    let mut st = e.state.lock();
                    if *st != IndexState::Ready {
                        *st = IndexState::Scanning;
                    }
                }
                e.progress.files.store(0, Ordering::Relaxed);
                e.progress.bytes.store(0, Ordering::Relaxed);
                e.progress.cancel.store(false, Ordering::Relaxed);
                *e.events_during_scan.lock() = Some(HashSet::new());
                // Keep the UI ticking for the live counter.
                let ticking = Arc::new(AtomicBool::new(true));
                {
                    let (ctx, ticking) = (e.ctx.clone(), ticking.clone());
                    std::thread::spawn(move || {
                        while ticking.load(Ordering::Relaxed) {
                            ctx.request_repaint();
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    });
                }
                let roots = e.roots.read().clone();
                let mut excludes = e.excludes.read().clone();
                excludes.push(data_dir().to_string_lossy().into_owned());
                let new_idx = Index::scan(&roots, &excludes, &e.progress);
                ticking.store(false, Ordering::Relaxed);
                if e.progress.cancel.load(Ordering::Relaxed) {
                    e.scanning.store(false, Ordering::SeqCst);
                    *e.events_during_scan.lock() = None;
                    return;
                }
                e.last_scan_ms.store(new_idx.scan_ms, Ordering::Relaxed);
                {
                    let mut idx = e.index.write();
                    *idx = new_idx;
                    // Replay changes that raced with the scan.
                    if let Some(paths) = e.events_during_scan.lock().take() {
                        for p in paths {
                            idx.reconcile(&p, &excludes);
                        }
                    }
                }
                *e.state.lock() = IndexState::Ready;
                e.scanning.store(false, Ordering::SeqCst);
                e.bump(true);
                e.save();
                release_free_memory();
            })
            .unwrap();
    }

    pub fn cancel_scan(&self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
    }

    /// Change the indexed locations; triggers a rescan.
    pub fn set_roots(self: &Arc<Self>, roots: Vec<String>, excludes: Vec<String>) {
        *self.roots.write() = roots;
        *self.excludes.write() = excludes;
        *self.watcher.lock() = None;
        self.start_watching();
        self.rescan();
    }

    /// Remove a path from the index right away (after deleting / trashing it).
    pub fn forget(&self, path: &std::path::Path) {
        let ex = self.excludes.read().clone();
        if self.index.write().reconcile(path, &ex) {
            self.bump(false);
        }
    }
}

/// Hand memory freed by the scan (the walk tree and the previous index) back to the OS.
fn release_free_memory() {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

fn search_worker(eng: Arc<Engine>, rx: Receiver<SearchRequest>, tx: Sender<SearchResponse>, labels: SharedLabels) {
    while let Ok(mut req) = rx.recv() {
        // Latest wins: skip stale keystrokes.
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        let q = Query::parse(&req.text, req.kind);
        let epoch = eng.epoch.load(Ordering::Relaxed);
        let out = {
            let idx = eng.index.read();
            search::run(&idx, &q, req.sort, req.desc, &labels)
        };
        let _ = tx.send(SearchResponse {
            seq: req.seq,
            ids: out.ids,
            elapsed_us: out.elapsed_us,
            highlight: q.highlight,
            epoch,
        });
        eng.ctx.request_repaint();
    }
}

fn event_worker(eng: Arc<Engine>, rx: Receiver<PathBuf>) {
    let data = data_dir();
    loop {
        let Ok(first) = rx.recv() else { return };
        let mut batch: HashSet<PathBuf> = HashSet::new();
        batch.insert(first);
        // Debounce: gather everything arriving within a short window.
        let deadline = Instant::now() + Duration::from_millis(250);
        while let Ok(p) = rx.recv_deadline(deadline) {
            batch.insert(p);
            if batch.len() > 100_000 {
                break;
            }
        }
        if batch.remove(&PathBuf::from("\0rescan")) {
            eng.rescan();
        }
        batch.retain(|p| !p.starts_with(&data));
        if batch.is_empty() {
            continue;
        }
        if let Some(pending) = eng.events_during_scan.lock().as_mut() {
            pending.extend(batch.iter().cloned());
        }
        let excludes = eng.excludes.read().clone();
        // Process parents before children so new trees are created top-down.
        let mut paths: Vec<PathBuf> = batch.into_iter().collect();
        paths.sort_by_key(|p| p.components().count());
        let mut changed = false;
        let mut compacted = false;
        {
            let mut idx = eng.index.write();
            for p in &paths {
                changed |= idx.reconcile(p, &excludes);
            }
            if changed {
                let before = idx.len();
                idx.compact_if_needed();
                compacted = idx.len() != before;
            }
        }
        if changed {
            eng.bump(compacted);
        }
    }
}
