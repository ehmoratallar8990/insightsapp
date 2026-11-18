//! The file index: a compact, cache-friendly struct-of-arrays representation of
//! every file system object, built with a parallel walker and persisted to disk so
//! the next launch is instant.
//!
//! Invariant: a parent always has a smaller id than its children. This holds for
//! the depth-first scan and for incremental appends (parents are created first),
//! and lets several algorithms (subtree deletion, folder size roll-ups) run as a
//! single linear pass.

use crate::filetype::{self, Kind};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const NONE: u32 = u32::MAX;
pub const F_DIR: u8 = 1;
pub const F_DELETED: u8 = 2;
pub const F_HIDDEN: u8 = 4;

const SNAPSHOT_VERSION: u32 = 3;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name_off: u32,
    pub name_len: u16,
    pub kind: u8,
    pub flags: u8,
    pub parent: u32,
    /// Seconds since the Unix epoch (0 = unknown).
    pub mtime: u32,
    /// Logical size in bytes.
    pub size: u64,
    /// Bytes actually allocated on disk (handles sparse files / compression).
    pub alloc: u64,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & F_DIR != 0
    }
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & F_DELETED != 0
    }
    #[inline]
    pub fn kind(&self) -> Kind {
        Kind::from_u8(self.kind)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    roots: Vec<String>,
    entries: Vec<Entry>,
    names: Vec<u8>,
    scanned_at: u64,
    scan_ms: u64,
}

pub struct Index {
    pub entries: Vec<Entry>,
    /// Original names (UTF-8, lossy-converted if needed).
    pub names: Vec<u8>,
    /// ASCII-lowercased copy of `names` (same offsets) for case-insensitive search.
    pub lower: Vec<u8>,
    pub roots: Vec<String>,
    lookup: HashMap<u64, u32>,
    root_ids: Vec<u32>,
    pub deleted: usize,
    pub scanned_at: u64,
    pub scan_ms: u64,
}

/// Shared progress counters for an in-flight scan.
#[derive(Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

#[inline]
fn key(parent: u32, lower_name: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&parent.to_le_bytes());
    h.update(lower_name);
    h.digest()
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn mtime_of(md: &std::fs::Metadata) -> u32 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs().min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

#[cfg(unix)]
fn alloc_of(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks() * 512
}
#[cfg(not(unix))]
fn alloc_of(md: &std::fs::Metadata) -> u64 {
    md.len()
}

use crate::walker::{self, DirNode, Stat};

/// Default roots: the whole machine.
pub fn default_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut v = Vec::new();
        for l in b'A'..=b'Z' {
            let p = format!("{}:\\", l as char);
            if Path::new(&p).exists() {
                v.push(p);
            }
        }
        if v.is_empty() {
            v.push("C:\\".into());
        }
        v
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

/// Paths that are virtual, duplicated (firmlinks) or not useful to index.
pub fn default_excludes() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    #[cfg(target_os = "macos")]
    v.extend(
        [
            "/System/Volumes",
            "/dev",
            "/Volumes",
            "/private/var/vm",
            "/private/var/db/uuidtext",
            "/.Spotlight-V100",
            "/.fseventsd",
            "/.DocumentRevisions-V100",
            "/.vol",
            "/cores",
        ]
        .map(String::from),
    );
    #[cfg(target_os = "linux")]
    v.extend(["/proc", "/sys", "/dev", "/run", "/snap", "/var/lib/docker/overlay2"].map(String::from));
    #[cfg(windows)]
    v.extend(["C:\\Windows\\WinSxS", "C:\\$Recycle.Bin", "C:\\System Volume Information"].map(String::from));
    v
}

impl Index {
    pub fn empty(roots: Vec<String>) -> Self {
        Index {
            entries: Vec::new(),
            names: Vec::new(),
            lower: Vec::new(),
            roots,
            lookup: HashMap::new(),
            root_ids: Vec::new(),
            deleted: 0,
            scanned_at: 0,
            scan_ms: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn live_count(&self) -> usize {
        self.entries.len() - self.deleted
    }

    #[inline]
    pub fn name(&self, id: u32) -> &str {
        let e = &self.entries[id as usize];
        let b = &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
        // Names are always stored as valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    }

    #[inline]
    pub fn lower_name(&self, id: u32) -> &[u8] {
        let e = &self.entries[id as usize];
        &self.lower[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }

    pub fn path_string(&self, id: u32) -> String {
        let mut chain = Vec::with_capacity(16);
        let mut cur = id;
        while cur != NONE {
            chain.push(cur);
            cur = self.entries[cur as usize].parent;
        }
        let mut s = String::with_capacity(chain.len() * 16);
        for (i, &c) in chain.iter().rev().enumerate() {
            let n = self.name(c);
            if i > 0 && !s.ends_with(std::path::MAIN_SEPARATOR) {
                s.push(std::path::MAIN_SEPARATOR);
            }
            s.push_str(n);
        }
        s
    }

    /// Lower-case full path of the *parent directory* written into `out`.
    pub fn write_lower_dir_path(&self, dir: u32, out: &mut Vec<u8>) {
        out.clear();
        if dir == NONE {
            return;
        }
        let mut chain: [u32; 128] = [0; 128];
        let mut n = 0;
        let mut cur = dir;
        while cur != NONE && n < chain.len() {
            chain[n] = cur;
            n += 1;
            cur = self.entries[cur as usize].parent;
        }
        let sep = std::path::MAIN_SEPARATOR as u8;
        for i in (0..n).rev() {
            let nm = self.lower_name(chain[i]);
            if i + 1 < n && out.last() != Some(&sep) {
                out.push(sep);
            }
            out.extend_from_slice(nm);
        }
    }

    fn push_name(&mut self, name: &str) -> (u32, u16) {
        let off = self.names.len() as u32;
        let bytes = name.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.names.extend_from_slice(&bytes[..len]);
        self.lower.extend(bytes[..len].iter().map(|b| b.to_ascii_lowercase()));
        (off, len as u16)
    }

    fn push_entry(&mut self, parent: u32, name: &str, is_dir: bool, st: Stat) -> u32 {
        let (off, len) = self.push_name(name);
        let lower = &self.lower[off as usize..off as usize + len as usize];
        let kind = filetype::classify(lower, is_dir) as u8;
        let mut flags = if is_dir { F_DIR } else { 0 };
        if name.starts_with('.') {
            flags |= F_HIDDEN;
        }
        let id = self.entries.len() as u32;
        self.entries.push(Entry {
            name_off: off,
            name_len: len,
            kind,
            flags,
            parent,
            mtime: st.mtime,
            size: if is_dir { 0 } else { st.size },
            alloc: if is_dir { 0 } else { st.alloc },
        });
        id
    }

    fn rebuild_lookup(&mut self) {
        let mut map = HashMap::with_capacity(self.entries.len());
        self.root_ids.clear();
        for (i, e) in self.entries.iter().enumerate() {
            if e.is_deleted() {
                continue;
            }
            if e.parent == NONE {
                self.root_ids.push(i as u32);
            }
            let ln = &self.lower[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            map.insert(key(e.parent, ln), i as u32);
        }
        self.lookup = map;
    }

    /// Append a walked tree depth-first so parents always precede children.
    fn append_tree(&mut self, node: &DirNode, parent: u32) {
        let mut sub = node.subdirs.iter();
        for it in &node.items {
            let id = self.push_entry(parent, node.name(it), it.is_dir, it.stat);
            if it.is_dir {
                if let Some(child) = sub.next() {
                    self.append_tree(child, id);
                }
            }
        }
    }

    /// Walk `root` (already present as `root_id`) and append everything below it.
    fn walk_into(&mut self, root: &Path, root_id: u32, excludes: &[PathBuf], progress: &ScanProgress) {
        let ctx = walker::WalkCtx { excludes, files: &progress.files, bytes: &progress.bytes, cancel: &progress.cancel };
        // A dedicated pool keeps the global rayon pool free for searches and
        // analysis while a (re)scan is running.
        static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
        let pool = POOL.get_or_init(|| {
            let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            rayon::ThreadPoolBuilder::new()
                .num_threads(n) // one per core; more threads only add kernel lock contention
                .thread_name(|i| format!("walker-{i}"))
                .build()
                .expect("walker pool")
        });
        let tree = pool.install(|| walker::walk(root, &ctx));
        self.append_tree(&tree, root_id);
    }

    /// Full scan of all roots.
    pub fn scan(roots: &[String], excludes: &[String], progress: &ScanProgress) -> Index {
        let t = Instant::now();
        let mut idx = Index::empty(roots.to_vec());
        idx.entries.reserve(1 << 20);
        idx.names.reserve(24 << 20);
        idx.lower.reserve(24 << 20);
        let ex: Vec<PathBuf> = excludes.iter().map(PathBuf::from).collect();
        for r in roots {
            let p = Path::new(r);
            let st = std::fs::metadata(p).map(|m| Stat { size: 0, alloc: 0, mtime: mtime_of(&m) }).unwrap_or_default();
            let root_id = idx.push_entry(NONE, r, true, st);
            idx.walk_into(p, root_id, &ex, progress);
        }
        // Growth slack can be ~2x after the scan; give it back.
        idx.entries.shrink_to_fit();
        idx.names.shrink_to_fit();
        idx.lower.shrink_to_fit();
        idx.rebuild_lookup();
        idx.scanned_at = now_secs();
        idx.scan_ms = t.elapsed().as_millis() as u64;
        idx
    }

    fn root_for(&self, path: &Path) -> Option<(u32, PathBuf)> {
        // Longest matching root wins.
        let mut best: Option<(u32, usize)> = None;
        for &i in &self.root_ids {
            let r = self.name(i);
            if path.starts_with(r) && best.is_none_or(|(_, l)| r.len() > l) {
                best = Some((i, r.len()));
            }
        }
        best.map(|(id, _)| (id, PathBuf::from(self.name(id))))
    }

    /// Resolve an absolute path to an entry id.
    pub fn lookup(&self, path: &Path) -> Option<u32> {
        let (mut cur, root) = self.root_for(path)?;
        let rel = path.strip_prefix(&root).ok()?;
        for comp in rel.components() {
            let s = comp.as_os_str().to_string_lossy();
            let lower: Vec<u8> = s.as_bytes().iter().map(|b| b.to_ascii_lowercase()).collect();
            let k = key(cur, &lower);
            let id = *self.lookup.get(&k)?;
            if self.name(id) != s {
                // Case-insensitive file systems: accept the case-folded match.
                if !self.name(id).eq_ignore_ascii_case(&s) {
                    return None;
                }
            }
            cur = id;
        }
        Some(cur)
    }

    fn mark_deleted(&mut self, id: u32) {
        let e = self.entries[id as usize];
        if e.is_deleted() {
            return;
        }
        let ln = self.lower_name(id).to_vec();
        self.lookup.remove(&key(e.parent, &ln));
        self.entries[id as usize].flags |= F_DELETED;
        self.deleted += 1;
        if e.is_dir() {
            // Children always have larger ids than their parent: one forward pass.
            for i in (id as usize + 1)..self.entries.len() {
                let p = self.entries[i].parent;
                if p != NONE && self.entries[p as usize].is_deleted() && !self.entries[i].is_deleted() {
                    let ln = self.lower_name(i as u32).to_vec();
                    self.lookup.remove(&key(p, &ln));
                    self.entries[i].flags |= F_DELETED;
                    self.deleted += 1;
                }
            }
        }
    }

    /// Bring the entry for `path` in sync with the file system. Returns true if
    /// anything changed.
    pub fn reconcile(&mut self, path: &Path, excludes: &[String]) -> bool {
        if excludes.iter().any(|x| path.starts_with(x)) {
            return false;
        }
        let existing = self.lookup(path);
        match std::fs::symlink_metadata(path) {
            Err(_) => {
                if let Some(id) = existing {
                    self.mark_deleted(id);
                    return true;
                }
                false
            }
            Ok(md) => {
                let st = Stat { size: md.len(), alloc: alloc_of(&md), mtime: mtime_of(&md) };
                if let Some(id) = existing {
                    let e = &mut self.entries[id as usize];
                    let was_dir = e.is_dir();
                    if was_dir == md.is_dir() {
                        if !was_dir {
                            e.size = st.size;
                            e.alloc = st.alloc;
                        }
                        e.mtime = st.mtime;
                        return true;
                    }
                    // Type changed (file <-> dir): replace.
                    self.mark_deleted(id);
                }
                // Ensure the parent exists (creating missing ancestors).
                let Some(parent_path) = path.parent() else { return false };
                let parent = match self.lookup(parent_path) {
                    Some(p) => p,
                    None => {
                        if !self.reconcile(parent_path, excludes) {
                            return false;
                        }
                        match self.lookup(parent_path) {
                            Some(p) => p,
                            None => return false,
                        }
                    }
                };
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let is_dir = md.is_dir();
                let id = self.push_entry(parent, &name, is_dir, st);
                let ln = self.lower_name(id).to_vec();
                self.lookup.insert(key(parent, &ln), id);
                if is_dir && !md.file_type().is_symlink() {
                    let start = self.entries.len();
                    let ex: Vec<PathBuf> = excludes.iter().map(PathBuf::from).collect();
                    let prog = ScanProgress::default();
                    self.walk_into(path, id, &ex, &prog);
                    for i in start..self.entries.len() {
                        let e = self.entries[i];
                        let ln = self.lower_name(i as u32).to_vec();
                        self.lookup.insert(key(e.parent, &ln), i as u32);
                    }
                }
                true
            }
        }
    }

    /// Rebuild without tombstones when they pile up.
    pub fn compact_if_needed(&mut self) {
        if self.deleted < 50_000 || self.deleted * 5 < self.entries.len() {
            return;
        }
        let mut remap = vec![NONE; self.entries.len()];
        let mut out = Index::empty(self.roots.clone());
        out.scanned_at = self.scanned_at;
        out.scan_ms = self.scan_ms;
        for i in 0..self.entries.len() {
            let e = self.entries[i];
            if e.is_deleted() {
                continue;
            }
            let parent = if e.parent == NONE { NONE } else { remap[e.parent as usize] };
            if e.parent != NONE && parent == NONE {
                continue;
            }
            let name = self.name(i as u32).to_string();
            let (off, len) = out.push_name(&name);
            remap[i] = out.entries.len() as u32;
            out.entries.push(Entry { name_off: off, name_len: len, parent, ..e });
        }
        out.rebuild_lookup();
        *self = out;
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let f = std::fs::File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            // Serialize without cloning the big arrays.
            #[derive(Serialize)]
            struct SnapRef<'a> {
                version: u32,
                roots: &'a Vec<String>,
                entries: &'a Vec<Entry>,
                names: &'a Vec<u8>,
                scanned_at: u64,
                scan_ms: u64,
            }
            // Tombstones are kept so ids stay stable for the parent chain.
            let entries = &self.entries;
            bincode::serialize_into(
                &mut w,
                &SnapRef {
                    version: SNAPSHOT_VERSION,
                    roots: &self.roots,
                    entries,
                    names: &self.names,
                    scanned_at: self.scanned_at,
                    scan_ms: self.scan_ms,
                },
            )?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Index> {
        let f = std::fs::File::open(path).context("open index")?;
        let r = BufReader::with_capacity(1 << 20, f);
        let snap: Snapshot = bincode::deserialize_from(r).context("decode index")?;
        anyhow::ensure!(snap.version == SNAPSHOT_VERSION, "index version mismatch");
        let lower: Vec<u8> = snap.names.iter().map(|b| b.to_ascii_lowercase()).collect();
        let deleted = snap.entries.iter().filter(|e| e.is_deleted()).count();
        let mut idx = Index {
            entries: snap.entries,
            names: snap.names,
            lower,
            roots: snap.roots,
            lookup: HashMap::new(),
            root_ids: Vec::new(),
            deleted,
            scanned_at: snap.scanned_at,
            scan_ms: snap.scan_ms,
        };
        idx.rebuild_lookup();
        Ok(idx)
    }

    /// Recursive size of each directory (allocated bytes) and file counts.
    pub fn rollup(&self) -> (Vec<u64>, Vec<u32>) {
        let n = self.entries.len();
        let mut sizes = vec![0u64; n];
        let mut counts = vec![0u32; n];
        for i in (0..n).rev() {
            let e = &self.entries[i];
            if e.is_deleted() {
                continue;
            }
            if !e.is_dir() {
                sizes[i] = e.alloc;
                counts[i] = 1;
            }
            if e.parent != NONE {
                let p = e.parent as usize;
                sizes[p] += sizes[i];
                counts[p] += counts[i];
            }
        }
        (sizes, counts)
    }
}

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("INSIGHT_DATA_DIR") {
        return PathBuf::from(d);
    }
    dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("insight")
}

pub fn index_file() -> PathBuf {
    data_dir().join("index.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lookup_reconcile() {
        let dir = std::env::temp_dir().join(format!("insight-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/hello.txt"), b"hello").unwrap();
        std::fs::write(dir.join("a/photo.JPG"), vec![0u8; 2048]).unwrap();
        let root = dir.to_string_lossy().to_string();
        let prog = ScanProgress::default();
        let mut idx = Index::scan(&[root.clone()], &[], &prog);
        assert_eq!(idx.live_count(), 5);
        let id = idx.lookup(&dir.join("a/b/hello.txt")).expect("found");
        assert_eq!(PathBuf::from(idx.path_string(id)), dir.join("a").join("b").join("hello.txt"));
        assert_eq!(idx.entries[id as usize].size, 5);
        let pid = idx.lookup(&dir.join("a/photo.JPG")).unwrap();
        assert_eq!(idx.entries[pid as usize].kind(), Kind::Image);

        // Create + delete.
        std::fs::create_dir_all(dir.join("new/deep")).unwrap();
        std::fs::write(dir.join("new/deep/x.rs"), b"fn main(){}").unwrap();
        assert!(idx.reconcile(&dir.join("new"), &[]));
        assert!(idx.lookup(&dir.join("new/deep/x.rs")).is_some());
        std::fs::remove_dir_all(dir.join("a")).unwrap();
        assert!(idx.reconcile(&dir.join("a"), &[]));
        assert!(idx.lookup(&dir.join("a/b/hello.txt")).is_none());
        assert_eq!(idx.live_count(), 4);

        // Persistence round-trip.
        let f = dir.join("idx.bin");
        idx.save(&f).unwrap();
        let back = Index::load(&f).unwrap();
        assert_eq!(back.live_count(), idx.live_count());
        assert!(back.lookup(&dir.join("new/deep/x.rs")).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
