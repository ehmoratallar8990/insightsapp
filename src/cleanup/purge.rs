//! `mo purge`-style project artifact cleanup, answered straight from the file index
//! (no disk walk): `node_modules`, Rust `target`, SwiftPM `.build`, `build`/`dist`
//! and framework caches — only when the marker file proving they are rebuildable
//! sits next to them.

use crate::index::{Index, NONE, now_secs};

#[derive(Clone, Debug)]
pub struct Artifact {
    pub path: String,
    pub project: String,
    pub kind: &'static str,
    pub bytes: u64,
    pub files: u32,
    /// Seconds since anything inside changed.
    pub idle_secs: u64,
    pub on: bool,
}

/// (dir name, label, sibling markers — any one suffices)
const RULES: &[(&str, &str, &[&str])] = &[
    ("node_modules", "Node modules", &["package.json"]),
    ("target", "Rust / Maven build", &["Cargo.toml", "pom.xml"]),
    (".build", "SwiftPM build", &["Package.swift"]),
    ("build", "Build output", &["build.gradle", "build.gradle.kts", "CMakeLists.txt", "package.json", "pyproject.toml", "setup.py", "pubspec.yaml"]),
    ("dist", "Dist bundle", &["package.json", "pyproject.toml", "setup.py"]),
    (".next", "Next.js cache", &["package.json"]),
    (".nuxt", "Nuxt cache", &["package.json"]),
    (".turbo", "Turborepo cache", &["package.json", "turbo.json"]),
    (".parcel-cache", "Parcel cache", &["package.json"]),
    (".svelte-kit", "SvelteKit output", &["package.json"]),
    (".dart_tool", "Dart tool cache", &["pubspec.yaml"]),
    ("Pods", "CocoaPods", &["Podfile"]),
    (".gradle", "Gradle project cache", &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"]),
];

pub const RECENT_DAYS: u64 = 7;

/// Find artifacts under the home folder. `sizes` / `counts` come from `Index::rollup`.
pub fn find(idx: &Index, sizes: &[u64], counts: &[u32]) -> Vec<Artifact> {
    let home = super::home();
    let home_id = idx.lookup(&home);
    let n = idx.len();
    // Newest mtime per subtree (children have larger ids than parents).
    let mut newest = vec![0u32; n];
    for i in (0..n).rev() {
        let e = &idx.entries[i];
        if e.is_deleted() {
            continue;
        }
        newest[i] = newest[i].max(e.mtime);
        if e.parent != NONE {
            let p = e.parent as usize;
            newest[p] = newest[p].max(newest[i]);
        }
    }
    let now = now_secs();
    let mut out = Vec::new();
    'entries: for (i, e) in idx.entries.iter().enumerate() {
        if e.is_deleted() || !e.is_dir() || e.parent == NONE {
            continue;
        }
        let name = idx.name(i as u32);
        let Some(&(_, label, markers)) = RULES.iter().find(|(n, _, _)| *n == name) else { continue };
        if !markers.iter().any(|m| idx.child(e.parent, m).is_some()) {
            continue;
        }
        // Must live under home, outside hidden tool dirs, app bundles, Library and
        // other artifacts (only the outermost node_modules counts).
        let mut cur = e.parent;
        let mut under_home = home_id.is_none();
        while cur != NONE {
            if Some(cur) == home_id {
                under_home = true;
                break;
            }
            let an = idx.name(cur);
            if (an.starts_with('.') && an.len() > 1)
                || an.ends_with(".app")
                || an == "Library"
                || an == "AppData"
                || RULES.iter().any(|(r, _, _)| *r == an)
            {
                continue 'entries;
            }
            cur = idx.entries[cur as usize].parent;
        }
        if !under_home {
            continue;
        }
        let bytes = sizes[i];
        if bytes < 1 << 20 {
            continue;
        }
        let idle = now.saturating_sub(newest[i] as u64);
        let parent_path = idx.path_string(e.parent);
        let project = std::path::Path::new(&parent_path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(Artifact {
            path: idx.path_string(i as u32),
            project,
            kind: label,
            bytes,
            files: counts[i],
            idle_secs: idle,
            on: idle > RECENT_DAYS * 86_400,
        });
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    out
}

/// Permanently delete the ticked artifacts (they are rebuildable by definition).
pub fn purge(items: &[Artifact]) -> (u64, Vec<String>) {
    use rayon::prelude::*;
    let res: Vec<(u64, Option<String>)> = items
        .par_iter()
        .filter(|a| a.on)
        .map(|a| match super::remove(std::path::Path::new(&a.path)) {
            Ok(()) => (a.bytes, None),
            Err(e) => (0, Some(format!("{}: {e}", a.path))),
        })
        .collect();
    let freed = res.iter().map(|r| r.0).sum();
    (freed, res.into_iter().filter_map(|r| r.1).collect())
}
