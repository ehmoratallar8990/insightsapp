//! Storage insights: where space goes, what can be reclaimed, duplicates.

use crate::filetype::Kind;
use crate::index::{Index, NONE, now_secs};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone)]
pub struct DiskInfo {
    pub name: String,
    pub mount: String,
    pub total: u64,
    pub available: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    /// Regenerated automatically; safe to delete.
    Safe,
    /// Probably safe, but have a look first.
    Review,
}

#[derive(Clone)]
pub struct Suggestion {
    pub icon: &'static str,
    pub title: String,
    pub detail: &'static str,
    pub safety: Safety,
    pub bytes: u64,
    /// (path, bytes), biggest first.
    pub items: Vec<(String, u64)>,
}

pub struct StorageReport {
    pub epoch: u64,
    pub total_bytes: u64,
    pub total_files: u64,
    pub by_kind: Vec<(Kind, u64, u64)>,
    pub largest_files: Vec<(String, u64, u32)>,
    pub largest_dirs: Vec<(String, u64, u32)>,
    pub suggestions: Vec<Suggestion>,
    pub disks: Vec<DiskInfo>,
    /// Recursive sizes per entry id (for the treemap).
    pub dir_sizes: Vec<u64>,
    /// CSR children lists: children of `i` are `children[off[i]..off[i+1]]`.
    pub child_off: Vec<u32>,
    pub children: Vec<u32>,
    pub roots: Vec<u32>,
    pub elapsed_ms: u64,
}

impl StorageReport {
    pub fn children_of(&self, id: u32) -> &[u32] {
        let a = self.child_off[id as usize] as usize;
        let b = self.child_off[id as usize + 1] as usize;
        &self.children[a..b]
    }
}

pub fn disks() -> Vec<DiskInfo> {
    let Ok(list) = sysinfo::Disks::new_with_refreshed_list() else { return Vec::new() };
    let mut v: Vec<DiskInfo> = list
        .iter()
        .filter(|d| d.total_space() > 1 << 30)
        .map(|d| DiskInfo {
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_string_lossy().into_owned(),
            total: d.total_space(),
            available: d.available_space(),
        })
        .collect();
    // APFS exposes several volumes of the same container; keep unique (total, available).
    v.sort_by(|a, b| a.mount.len().cmp(&b.mount.len()));
    let mut seen = std::collections::HashSet::new();
    v.retain(|d| seen.insert((d.total, d.available / (64 << 20))));
    v
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub fn analyze(idx: &Index, epoch: u64) -> StorageReport {
    let t = std::time::Instant::now();
    let n = idx.len();
    let (dir_sizes, counts) = idx.rollup();

    // Children CSR.
    let mut child_off = vec![0u32; n + 1];
    for e in &idx.entries {
        if !e.is_deleted() && e.parent != NONE {
            child_off[e.parent as usize + 1] += 1;
        }
    }
    for i in 0..n {
        child_off[i + 1] += child_off[i];
    }
    let mut fill = child_off.clone();
    let mut children = vec![0u32; child_off[n] as usize];
    let mut roots = Vec::new();
    for (i, e) in idx.entries.iter().enumerate() {
        if e.is_deleted() {
            continue;
        }
        if e.parent == NONE {
            roots.push(i as u32);
        } else {
            let p = e.parent as usize;
            children[fill[p] as usize] = i as u32;
            fill[p] += 1;
        }
    }

    // Totals by kind.
    let mut by_kind: HashMap<Kind, (u64, u64)> = HashMap::new();
    let mut total_bytes = 0u64;
    let mut total_files = 0u64;
    for e in &idx.entries {
        if e.is_deleted() || e.is_dir() {
            continue;
        }
        let k = by_kind.entry(e.kind()).or_default();
        k.0 += e.alloc;
        k.1 += 1;
        total_bytes += e.alloc;
        total_files += 1;
    }
    let mut by_kind: Vec<(Kind, u64, u64)> = by_kind.into_iter().map(|(k, (b, c))| (k, b, c)).collect();
    by_kind.sort_by(|a, b| b.1.cmp(&a.1));

    // Largest files.
    let mut files: Vec<u32> = (0..n as u32)
        .into_par_iter()
        .filter(|&i| {
            let e = &idx.entries[i as usize];
            !e.is_deleted() && !e.is_dir() && e.alloc > 50 << 20
        })
        .collect();
    files.par_sort_unstable_by_key(|&i| std::cmp::Reverse(idx.entries[i as usize].alloc));
    let largest_files = files
        .iter()
        .take(200)
        .map(|&i| (idx.path_string(i), idx.entries[i as usize].alloc, idx.entries[i as usize].mtime))
        .collect();

    // Largest leaf-ish folders: big folders whose biggest child folder holds < 60% of them,
    // so we surface the folder that actually *contains* the bulk, not every ancestor.
    let mut dirs: Vec<u32> = (0..n as u32)
        .into_par_iter()
        .filter(|&i| {
            let e = &idx.entries[i as usize];
            if e.is_deleted() || !e.is_dir() || e.parent == NONE || dir_sizes[i as usize] < 200 << 20 {
                return false;
            }
            let a = child_off[i as usize] as usize;
            let b = child_off[i as usize + 1] as usize;
            let max_child = children[a..b]
                .iter()
                .filter(|&&c| idx.entries[c as usize].is_dir())
                .map(|&c| dir_sizes[c as usize])
                .max()
                .unwrap_or(0);
            (max_child as f64) < dir_sizes[i as usize] as f64 * 0.6
        })
        .collect();
    dirs.par_sort_unstable_by_key(|&i| std::cmp::Reverse(dir_sizes[i as usize]));
    let largest_dirs = dirs
        .iter()
        .take(100)
        .map(|&i| (idx.path_string(i), dir_sizes[i as usize], counts[i as usize]))
        .collect();

    let suggestions = suggestions(idx, &dir_sizes, &child_off, &children);

    StorageReport {
        epoch,
        total_bytes,
        total_files,
        by_kind,
        largest_files,
        largest_dirs,
        suggestions,
        disks: disks(),
        dir_sizes,
        child_off,
        children,
        roots,
        elapsed_ms: t.elapsed().as_millis() as u64,
    }
}

fn suggestions(idx: &Index, sizes: &[u64], child_off: &[u32], children: &[u32]) -> Vec<Suggestion> {
    let h = home();
    let mut out = Vec::new();
    let path_size = |p: &Path| -> Option<(String, u64)> {
        let id = idx.lookup(p)?;
        Some((p.to_string_lossy().into_owned(), sizes[id as usize]))
    };
    let mut add_paths = |icon, title: &str, detail, safety, paths: Vec<PathBuf>| {
        let mut items: Vec<(String, u64)> = paths.iter().filter_map(|p| path_size(p)).filter(|(_, s)| *s > 0).collect();
        items.sort_by(|a, b| b.1.cmp(&a.1));
        let bytes = items.iter().map(|i| i.1).sum();
        if bytes > 0 {
            out.push(Suggestion { icon, title: title.to_string(), detail, safety, bytes, items });
        }
    };

    // Well-known locations.
    #[cfg(target_os = "macos")]
    {
        add_paths(
            "💾",
            "App caches",
            "Applications rebuild these on demand. Quit apps before cleaning.",
            Safety::Safe,
            vec![h.join("Library/Caches")],
        );
        add_paths(
            "⚙",
            "Xcode build data",
            "DerivedData and old device support files are recreated by Xcode.",
            Safety::Safe,
            vec![
                h.join("Library/Developer/Xcode/DerivedData"),
                h.join("Library/Developer/Xcode/iOS DeviceSupport"),
                h.join("Library/Developer/Xcode/watchOS DeviceSupport"),
                h.join("Library/Developer/CoreSimulator/Caches"),
            ],
        );
        add_paths(
            "📱",
            "Simulators & archives",
            "Old iOS simulators and app archives. Remove unused ones from Xcode.",
            Safety::Review,
            vec![h.join("Library/Developer/CoreSimulator/Devices"), h.join("Library/Developer/Xcode/Archives")],
        );
        add_paths(
            "💾",
            "iPhone / iPad backups",
            "Local device backups. Keep the latest if you don't use iCloud backup.",
            Safety::Review,
            vec![h.join("Library/Application Support/MobileSync/Backup")],
        );
        add_paths(
            "🚀",
            "Docker disk image",
            "Run `docker system prune` to shrink it, or lower the disk limit in Docker settings.",
            Safety::Review,
            vec![h.join("Library/Containers/com.docker.docker/Data/vms")],
        );
        add_paths("🗑", "Trash", "Files already in the Trash still use space.", Safety::Safe, vec![h.join(".Trash")]);
        add_paths("📜", "System & app logs", "Old log files.", Safety::Safe, vec![h.join("Library/Logs")]);
    }
    #[cfg(target_os = "linux")]
    {
        add_paths("💾", "User caches", "Applications rebuild these on demand.", Safety::Safe, vec![h.join(".cache")]);
        add_paths("🗑", "Trash", "Files already in the Trash still use space.", Safety::Safe, vec![h.join(".local/share/Trash")]);
        add_paths(
            "📦",
            "Package caches",
            "Downloaded packages. Clean with `sudo apt clean` / `dnf clean all`.",
            Safety::Safe,
            vec![PathBuf::from("/var/cache/apt/archives"), PathBuf::from("/var/cache/dnf")],
        );
        add_paths("📜", "System logs", "Trim with `journalctl --vacuum-size=200M`.", Safety::Review, vec![PathBuf::from("/var/log")]);
    }
    #[cfg(windows)]
    {
        let local = dirs::data_local_dir().unwrap_or_default();
        add_paths("💾", "Temporary files", "Safe to remove when apps are closed.", Safety::Safe, vec![local.join("Temp")]);
        add_paths(
            "💾",
            "Browser & app caches",
            "Rebuilt on demand.",
            Safety::Safe,
            vec![
                local.join("Google\\Chrome\\User Data\\Default\\Cache"),
                local.join("Microsoft\\Edge\\User Data\\Default\\Cache"),
                local.join("Microsoft\\Windows\\INetCache"),
            ],
        );
        add_paths("🗑", "Recycle Bin", "Empty the Recycle Bin.", Safety::Safe, vec![PathBuf::from("C:\\$Recycle.Bin")]);
        add_paths("🗑", "Windows update leftovers", "Use Disk Cleanup → 'Clean up system files'.", Safety::Review, vec![PathBuf::from("C:\\Windows\\SoftwareDistribution\\Download")]);
    }
    add_paths(
        "💾",
        "Developer tool caches",
        "Package manager caches (npm, yarn, pip, cargo, gradle, maven). Re-downloaded when needed.",
        Safety::Safe,
        vec![
            h.join(".npm/_cacache"),
            h.join("Library/Caches/Yarn"),
            h.join(".cache/yarn"),
            h.join(".cache/pip"),
            h.join("Library/Caches/pip"),
            h.join(".cargo/registry/cache"),
            h.join(".gradle/caches"),
            h.join(".m2/repository"),
            h.join("Library/pnpm/store"),
            h.join(".local/share/pnpm/store"),
            h.join("Library/Caches/Homebrew"),
            h.join("AppData\\Local\\npm-cache"),
        ],
    );

    // Pattern-based: project build artefacts / dependency folders, outermost only.
    let name_rules: &[(&[u8], &'static str, &'static str, &'static str, Option<&[u8]>)] = &[
        (b"node_modules", "📦", "node_modules folders", "JavaScript dependencies — restore with `npm install`.", None),
        (b"target", "⚙", "Rust build folders", "Cargo `target/` directories — rebuilt by `cargo build`.", Some(b"cargo.toml")),
        (b"__pycache__", "🐍", "Python bytecode caches", "Recreated automatically.", None),
        (b".venv", "🐍", "Python virtual environments", "Recreate with `python -m venv` + `pip install`.", None),
        (b".next", "📦", "Next.js build caches", "Rebuilt by `next build`.", None),
        (b".gradle", "📦", "Gradle project caches", "Rebuilt by Gradle.", None),
        (b"DerivedData", "⚙", "Stray DerivedData", "Xcode build products.", None),
    ];
    let lower_rules: Vec<Vec<u8>> = name_rules.iter().map(|r| r.0.to_ascii_lowercase()).collect();
    for (ri, rule) in name_rules.iter().enumerate() {
        let target = &lower_rules[ri];
        let mut items: Vec<(u32, u64)> = (0..idx.len() as u32)
            .into_par_iter()
            .filter(|&i| {
                let e = &idx.entries[i as usize];
                if e.is_deleted() || !e.is_dir() || idx.lower_name(i) != target.as_slice() || sizes[i as usize] < 1 << 20 {
                    return false;
                }
                // Skip nested matches (e.g. node_modules inside node_modules).
                let mut p = e.parent;
                while p != NONE {
                    if idx.lower_name(p) == target.as_slice() {
                        return false;
                    }
                    p = idx.entries[p as usize].parent;
                }
                // Optional sibling marker (e.g. Cargo.toml next to target/).
                if let Some(marker) = rule.4 {
                    let par = e.parent;
                    if par == NONE {
                        return false;
                    }
                    let a = child_off[par as usize] as usize;
                    let b = child_off[par as usize + 1] as usize;
                    if !children[a..b].iter().any(|&c| idx.lower_name(c) == marker) {
                        return false;
                    }
                }
                true
            })
            .map(|i| (i, sizes[i as usize]))
            .collect();
        if items.is_empty() {
            continue;
        }
        items.sort_by(|a, b| b.1.cmp(&a.1));
        let bytes = items.iter().map(|i| i.1).sum();
        out.push(Suggestion {
            icon: rule.1,
            title: format!("{} ({})", rule.2, items.len()),
            detail: rule.3,
            safety: Safety::Safe,
            bytes,
            items: items.into_iter().take(300).map(|(i, s)| (idx.path_string(i), s)).collect(),
        });
    }

    // Installers & disk images lying in Downloads.
    if let Some(dl) = idx.lookup(&h.join("Downloads")) {
        let exts: [&[u8]; 9] = [b"dmg", b"pkg", b"iso", b"exe", b"msi", b"deb", b"rpm", b"appimage", b"xip"];
        let mut items: Vec<(String, u64)> = Vec::new();
        let mut stack = vec![dl];
        while let Some(d) = stack.pop() {
            let a = child_off[d as usize] as usize;
            let b = child_off[d as usize + 1] as usize;
            for &c in &children[a..b] {
                let e = &idx.entries[c as usize];
                if e.is_dir() {
                    stack.push(c);
                } else {
                    let ln = idx.lower_name(c);
                    if let Some(p) = memchr::memrchr(b'.', ln) {
                        if exts.contains(&&ln[p + 1..]) {
                            items.push((idx.path_string(c), e.alloc));
                        }
                    }
                }
            }
        }
        items.sort_by(|a, b| b.1.cmp(&a.1));
        let bytes = items.iter().map(|i| i.1).sum();
        if bytes > 0 {
            out.push(Suggestion {
                icon: "💿",
                title: format!("Installers in Downloads ({})", items.len()),
                detail: "Disk images and installers you've probably already installed.",
                safety: Safety::Review,
                bytes,
                items,
            });
        }
    }

    // Large files untouched for over a year in the home folder.
    if let Some(home_id) = idx.lookup(&h) {
        let cutoff = (now_secs() as u32).saturating_sub(365 * 86_400);
        let home_prefix = h.to_string_lossy().to_lowercase();
        let mut items: Vec<(u32, u64)> = (0..idx.len() as u32)
            .into_par_iter()
            .filter(|&i| {
                let e = &idx.entries[i as usize];
                !e.is_deleted() && !e.is_dir() && e.alloc > 200 << 20 && e.mtime > 0 && e.mtime < cutoff && i > home_id
            })
            .map(|i| (i, idx.entries[i as usize].alloc))
            .collect();
        items.retain(|(i, _)| {
            let p = idx.path_string(*i).to_lowercase();
            p.starts_with(&home_prefix) && !p.contains("/library/") && !p.contains("\\appdata\\")
        });
        items.sort_by(|a, b| b.1.cmp(&a.1));
        let bytes = items.iter().map(|i| i.1).sum();
        if bytes > 0 {
            out.push(Suggestion {
                icon: "📁",
                title: format!("Large files untouched for 1+ year ({})", items.len()),
                detail: "Consider archiving these to an external drive or the cloud.",
                safety: Safety::Review,
                items: items.into_iter().take(300).map(|(i, s)| (idx.path_string(i), s)).collect(),
                bytes,
            });
        }
    }

    out.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    out
}

// ---------------------------------------------------------------------------
// Duplicate finder

#[derive(Default)]
pub struct DupProgress {
    pub stage: AtomicU64, // 0 idle, 1 grouping, 2 sampling, 3 hashing, 4 done
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

#[derive(Clone)]
pub struct DupGroup {
    pub size: u64,
    pub paths: Vec<String>,
}

impl DupGroup {
    pub fn wasted(&self) -> u64 {
        self.size * (self.paths.len() as u64 - 1)
    }
}

fn sample_hash(path: &Path, size: u64) -> Option<u64> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 16 * 1024];
    let positions: [u64; 3] = [0, size / 2, size.saturating_sub(buf.len() as u64)];
    for pos in positions {
        f.seek(SeekFrom::Start(pos)).ok()?;
        let n = f.read(&mut buf).ok()?;
        h.update(&buf[..n]);
    }
    Some(h.digest())
}

fn full_hash(path: &Path, cancel: &AtomicBool) -> Option<u128> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(h.digest128())
}

#[cfg(unix)]
fn inode(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn inode(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Find byte-identical files under `scope` that are at least `min_size` bytes.
/// Collects candidates under the read lock, then hashes without holding it.
pub fn find_duplicates(candidates: Vec<(String, u64)>, prog: &DupProgress) -> Vec<DupGroup> {
    prog.stage.store(1, Ordering::Relaxed);
    let mut by_size: HashMap<u64, Vec<String>> = HashMap::new();
    for (p, s) in candidates {
        by_size.entry(s).or_default().push(p);
    }
    by_size.retain(|_, v| v.len() > 1);

    // Stage 2: cheap sampled hash.
    let sized: Vec<(u64, String)> = by_size.into_iter().flat_map(|(s, v)| v.into_iter().map(move |p| (s, p))).collect();
    prog.stage.store(2, Ordering::Relaxed);
    prog.total.store(sized.len() as u64, Ordering::Relaxed);
    prog.done.store(0, Ordering::Relaxed);
    let sampled: Vec<(u64, u64, String)> = sized
        .into_par_iter()
        .filter_map(|(s, p)| {
            if prog.cancel.load(Ordering::Relaxed) {
                return None;
            }
            let h = sample_hash(Path::new(&p), s);
            prog.done.fetch_add(1, Ordering::Relaxed);
            h.map(|h| (s, h, p))
        })
        .collect();
    let mut groups: HashMap<(u64, u64), Vec<String>> = HashMap::new();
    for (s, h, p) in sampled {
        groups.entry((s, h)).or_default().push(p);
    }
    groups.retain(|_, v| v.len() > 1);

    // Drop hard links (same inode is the same file).
    for v in groups.values_mut() {
        let mut seen = std::collections::HashSet::new();
        v.retain(|p| inode(Path::new(p)).is_none_or(|i| seen.insert(i)));
    }
    groups.retain(|_, v| v.len() > 1);

    // Stage 3: full content hash.
    let todo: Vec<(u64, String)> = groups.into_iter().flat_map(|((s, _), v)| v.into_iter().map(move |p| (s, p))).collect();
    prog.stage.store(3, Ordering::Relaxed);
    prog.total.store(todo.iter().map(|t| t.0).sum(), Ordering::Relaxed);
    prog.done.store(0, Ordering::Relaxed);
    let hashed: Vec<(u64, u128, String)> = todo
        .into_par_iter()
        .filter_map(|(s, p)| {
            let h = full_hash(Path::new(&p), &prog.cancel);
            prog.done.fetch_add(s, Ordering::Relaxed);
            h.map(|h| (s, h, p))
        })
        .collect();
    let mut fin: HashMap<(u64, u128), Vec<String>> = HashMap::new();
    for (s, h, p) in hashed {
        fin.entry((s, h)).or_default().push(p);
    }
    let mut out: Vec<DupGroup> = fin
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((s, _), mut v)| {
            v.sort();
            DupGroup { size: s, paths: v }
        })
        .collect();
    out.sort_by(|a, b| b.wasted().cmp(&a.wasted()));
    prog.stage.store(4, Ordering::Relaxed);
    out
}

pub fn dup_candidates(idx: &Index, scope: &str, min_size: u64) -> Vec<(String, u64)> {
    let scope_l = scope.to_lowercase();
    let mut by_size: HashMap<u64, Vec<u32>> = HashMap::new();
    for (i, e) in idx.entries.iter().enumerate() {
        if !e.is_deleted() && !e.is_dir() && e.size >= min_size {
            by_size.entry(e.size).or_default().push(i as u32);
        }
    }
    by_size
        .into_par_iter()
        .filter(|(_, v)| v.len() > 1)
        .flat_map_iter(|(s, v)| {
            let scope_l = scope_l.clone();
            v.into_iter().filter_map(move |i| {
                let p = idx.path_string(i);
                let pl = p.to_lowercase();
                if pl.starts_with(&scope_l) && !pl.contains("/library/") && !pl.contains(".app/") && !pl.contains("/.git/") {
                    Some((p, s))
                } else {
                    None
                }
            })
        })
        .collect()
}

pub fn human(bytes: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.1} {}", U[i])
    }
}
