//! Fast parallel directory walker.
//!
//! * macOS: `getattrlistbulk(2)` returns names, types, sizes and dates for a whole
//!   directory in a handful of syscalls — no per-file `stat`. This is several
//!   times faster than `readdir` + `lstat`.
//! * Windows: `read_dir` metadata comes for free from `FindNextFileW`.
//! * Linux: `read_dir` + `statx`, parallelised.
//!
//! Directories are processed in parallel with rayon work-stealing; the result is
//! a tree that is flattened depth-first (parents before children) into the index.

use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, Default, Debug)]
pub struct Stat {
    pub size: u64,
    pub alloc: u64,
    pub mtime: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub name_off: u32,
    pub name_len: u16,
    pub is_dir: bool,
    pub stat: Stat,
}

/// One directory's listing plus its sub-directories (in the same order as the
/// directory items in `items`).
#[derive(Default, Debug)]
pub struct DirNode {
    pub names: String,
    pub items: Vec<Item>,
    pub subdirs: Vec<DirNode>,
}

impl DirNode {
    pub fn name(&self, it: &Item) -> &str {
        &self.names[it.name_off as usize..it.name_off as usize + it.name_len as usize]
    }
}

pub struct WalkCtx<'a> {
    pub excludes: &'a [PathBuf],
    pub files: &'a AtomicU64,
    pub bytes: &'a AtomicU64,
    pub cancel: &'a AtomicBool,
}

/// Recursively list `dir`.
pub fn walk(dir: &Path, ctx: &WalkCtx) -> DirNode {
    if ctx.cancel.load(Ordering::Relaxed) {
        return DirNode::default();
    }
    let mut node = list_dir(dir);
    // Drop excluded children.
    if !ctx.excludes.is_empty() {
        let names = node.names.clone();
        node.items.retain(|it| {
            let n = &names[it.name_off as usize..it.name_off as usize + it.name_len as usize];
            let p = dir.join(n);
            !ctx.excludes.iter().any(|x| p == *x)
        });
    }
    ctx.files.fetch_add(node.items.len() as u64, Ordering::Relaxed);
    ctx.bytes.fetch_add(node.items.iter().map(|i| i.stat.size).sum(), Ordering::Relaxed);
    let subdirs: Vec<PathBuf> = node.items.iter().filter(|i| i.is_dir).map(|i| dir.join(node.name(i))).collect();
    node.subdirs = if subdirs.len() > 1 {
        subdirs.par_iter().map(|p| walk(p, ctx)).collect()
    } else {
        subdirs.iter().map(|p| walk(p, ctx)).collect()
    };
    node
}

fn push(node: &mut DirNode, name: &str, is_dir: bool, stat: Stat) {
    if name.is_empty() || name == "." || name == ".." {
        return;
    }
    let off = node.names.len() as u32;
    let len = name.len().min(u16::MAX as usize);
    // Truncation must keep valid UTF-8.
    let mut cut = len;
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    node.names.push_str(&name[..cut]);
    node.items.push(Item { name_off: off, name_len: cut as u16, is_dir, stat });
}

#[cfg(target_os = "macos")]
fn list_dir(dir: &Path) -> DirNode {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    const VREG: u32 = 1;
    const VDIR: u32 = 2;

    let mut node = DirNode::default();
    let Ok(cpath) = CString::new(dir.as_os_str().as_bytes()) else { return node };
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return node;
    }
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | ATTR_CMN_ERROR
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_MODTIME;
    al.fileattr = libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = unsafe {
            libc::getattrlistbulk(fd, &mut al as *mut _ as *mut libc::c_void, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
        };
        if n <= 0 {
            break;
        }
        let mut entry = 0usize;
        for _ in 0..n {
            let base = buf.as_ptr();
            let rd_u32 = |off: usize| unsafe { std::ptr::read_unaligned(base.add(off) as *const u32) };
            let rd_i64 = |off: usize| unsafe { std::ptr::read_unaligned(base.add(off) as *const i64) };
            let len = rd_u32(entry) as usize;
            if len == 0 || entry + len > buf.len() {
                break;
            }
            let mut f = entry + 4;
            // attribute_set_t: commonattr, volattr, dirattr, fileattr, forkattr
            let common = rd_u32(f);
            let fileattr = rd_u32(f + 12);
            f += 20;
            let mut err = 0;
            if common & ATTR_CMN_ERROR != 0 {
                err = rd_u32(f);
                f += 4;
            }
            let mut name = std::borrow::Cow::Borrowed("");
            if common & libc::ATTR_CMN_NAME != 0 {
                let off = unsafe { std::ptr::read_unaligned(base.add(f) as *const i32) } as isize;
                let nlen = rd_u32(f + 4) as usize;
                let start = (f as isize + off) as usize;
                if nlen > 0 && start + nlen <= entry + len {
                    name = String::from_utf8_lossy(&buf[start..start + nlen - 1]); // drop NUL
                }
                f += 8;
            }
            let mut ty = 0;
            if common & libc::ATTR_CMN_OBJTYPE != 0 {
                ty = rd_u32(f);
                f += 4;
            }
            let mut mtime = 0u32;
            if common & libc::ATTR_CMN_MODTIME != 0 {
                mtime = rd_i64(f).clamp(0, u32::MAX as i64) as u32;
                f += 16;
            }
            let mut st = Stat { size: 0, alloc: 0, mtime };
            if ty == VREG {
                if fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
                    st.alloc = rd_i64(f).max(0) as u64;
                    f += 8;
                }
                if fileattr & libc::ATTR_FILE_DATALENGTH != 0 {
                    st.size = rd_i64(f).max(0) as u64;
                }
            }
            if err == 0 || !name.is_empty() {
                push(&mut node, &name, ty == VDIR, st);
            }
            entry += len;
        }
    }
    unsafe { libc::close(fd) };
    node
}

#[cfg(not(target_os = "macos"))]
fn list_dir(dir: &Path) -> DirNode {
    let mut node = DirNode::default();
    let Ok(rd) = std::fs::read_dir(dir) else { return node };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name();
        let name = name.to_string_lossy();
        // Windows: metadata is cached from the directory listing (no extra syscall).
        let st = match e.metadata() {
            Ok(md) => Stat {
                size: if ft.is_file() { md.len() } else { 0 },
                alloc: if ft.is_file() { alloc_of(&md) } else { 0 },
                mtime: md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs().min(u32::MAX as u64) as u32)
                    .unwrap_or(0),
            },
            Err(_) => Stat::default(),
        };
        // Never descend through symlinks / junctions.
        push(&mut node, &name, ft.is_dir() && !ft.is_symlink(), st);
    }
    node
}

#[cfg(all(unix, not(target_os = "macos")))]
fn alloc_of(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks() * 512
}
#[cfg(windows)]
fn alloc_of(md: &std::fs::Metadata) -> u64 {
    md.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_names_types_sizes() {
        let dir = std::env::temp_dir().join(format!("insight-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("a.txt"), b"12345").unwrap();
        std::fs::write(dir.join("sub/ünïcødé 名前.md"), vec![7u8; 10_000]).unwrap();
        let (f, b, c) = (AtomicU64::new(0), AtomicU64::new(0), AtomicBool::new(false));
        let ctx = WalkCtx { excludes: &[], files: &f, bytes: &b, cancel: &c };
        let root = walk(&dir, &ctx);
        let mut names: Vec<(&str, bool, u64)> = root.items.iter().map(|i| (root.name(i), i.is_dir, i.stat.size)).collect();
        names.sort();
        assert_eq!(names, vec![("a.txt", false, 5), ("sub", true, 0)]);
        let sub = &root.subdirs[0];
        let mut sn: Vec<(&str, u64)> = sub.items.iter().map(|i| (sub.name(i), i.stat.size)).collect();
        sn.sort();
        assert_eq!(sn, vec![("deeper", 0), ("ünïcødé 名前.md", 10_000)]);
        assert!(sub.items.iter().any(|i| i.stat.mtime > 1_600_000_000));
        assert_eq!(f.load(Ordering::Relaxed), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
