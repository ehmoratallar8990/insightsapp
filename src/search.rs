//! Query parsing and parallel matching.
//!
//! Syntax (all terms are ANDed, case-insensitive):
//!   foo bar          name contains "foo" and "bar"
//!   "foo bar"        phrase
//!   !foo  -foo       exclude
//!   jpg|png          any alternative
//!   *.rs  img_??.jpg wildcards (whole name)
//!   src/main         a term containing a path separator matches the full path
//!   path:proj  in:x  full path contains
//!   ext:jpg,png      extension
//!   type:image       folder|image|video|audio|doc|archive|code|app|other
//!   file: / folder:  only files / only folders
//!   size:>10mb  size:<1k  size:1mb..1gb
//!   dm:today|week|month|year|>2024-01-01|<2023-06
//!   label:dog        media label assigned by the on-device classifier
//!   re:^IMG_\d+      regular expression on the name
//!   hidden:yes|no    dot-files

use crate::filetype::Kind;
use crate::index::{F_HIDDEN, Index, NONE};
use memchr::memmem;
use rayon::prelude::*;
use std::collections::HashSet;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SortKey {
    #[default]
    Relevance,
    Name,
    Path,
    Size,
    Modified,
    Kind,
}

#[derive(Debug)]
enum Pred {
    /// Any of the alternatives is a substring of the name.
    Name(Vec<Vec<u8>>),
    /// Any of the alternatives is a substring of the full (lower-case) path.
    Path(Vec<Vec<u8>>),
    /// Regex plus an optional lower-case literal that must appear (cheap prefilter).
    Regex(regex::bytes::Regex, Option<Vec<u8>>),
    Ext(Vec<Vec<u8>>),
    Kinds(Vec<Kind>),
    Size(u64, u64),
    Mtime(u32, u32),
    Hidden(bool),
    OnlyFiles,
    OnlyDirs,
    Label(String),
}

#[derive(Debug)]
struct Clause {
    neg: bool,
    pred: Pred,
}

#[derive(Debug, Default)]
pub struct Query {
    clauses: Vec<Clause>,
    /// Plain (non-negated) name terms, used for relevance scoring + highlighting.
    pub highlight: Vec<String>,
    pub labels: Vec<String>,
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let idx = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let n: f64 = num.parse().ok()?;
    let m: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024f64.powi(4),
        _ => return None,
    };
    Some((n * m) as u64)
}

fn parse_range<T: Copy>(s: &str, parse: impl Fn(&str) -> Option<T>, min: T, max: T) -> Option<(T, T)> {
    if let Some((a, b)) = s.split_once("..") {
        let lo = if a.is_empty() { min } else { parse(a)? };
        let hi = if b.is_empty() { max } else { parse(b)? };
        return Some((lo, hi));
    }
    if let Some(r) = s.strip_prefix(">=").or_else(|| s.strip_prefix('>')) {
        return Some((parse(r)?, max));
    }
    if let Some(r) = s.strip_prefix("<=").or_else(|| s.strip_prefix('<')) {
        return Some((min, parse(r)?));
    }
    if let Some(r) = s.strip_prefix('=') {
        let v = parse(r)?;
        return Some((v, v));
    }
    None
}

fn parse_date(s: &str) -> Option<u32> {
    use chrono::{NaiveDate, TimeZone};
    let s = s.trim();
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d"))
        .or_else(|_| NaiveDate::parse_from_str(&format!("{s}-01-01"), "%Y-%m-%d"))
        .ok()?;
    let dt = chrono::Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).single()?;
    Some(dt.timestamp().max(0) as u32)
}

fn parse_dm(s: &str) -> Option<(u32, u32)> {
    let now = crate::index::now_secs() as u32;
    let day = 86_400;
    let start_of_today = {
        use chrono::{Local, Timelike};
        let n = Local::now();
        (n.timestamp() as u32).saturating_sub(n.num_seconds_from_midnight())
    };
    Some(match s.to_ascii_lowercase().as_str() {
        "today" => (start_of_today, u32::MAX),
        "yesterday" => (start_of_today - day, start_of_today),
        "week" | "thisweek" | "7d" => (now - 7 * day, u32::MAX),
        "month" | "thismonth" | "30d" => (now - 30 * day, u32::MAX),
        "year" | "thisyear" | "365d" => (now - 365 * day, u32::MAX),
        "old" => (0, now - 365 * day),
        other => {
            if let Some(r) = parse_range(other, parse_date, 0, u32::MAX) {
                r
            } else {
                let d = parse_date(other)?;
                (d, d + day)
            }
        }
    })
}

/// Split on whitespace, honouring double quotes.
fn tokenize(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inq = false;
    for c in q.chars() {
        match c {
            '"' => inq = !inq,
            c if c.is_whitespace() && !inq => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn glob_to_regex(g: &str) -> Option<regex::bytes::Regex> {
    let mut r = String::from("(?i)^");
    for c in g.chars() {
        match c {
            '*' => r.push_str(".*"),
            '?' => r.push('.'),
            c => r.push_str(&regex::escape(&c.to_string())),
        }
    }
    r.push('$');
    regex::bytes::Regex::new(&r).ok()
}

fn alts(v: &str) -> Vec<Vec<u8>> {
    v.split(['|', ','])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase().into_bytes())
        .collect()
}

impl Query {
    pub fn parse(q: &str, kind_filter: Option<Kind>) -> Query {
        let mut out = Query::default();
        if let Some(k) = kind_filter {
            out.clauses.push(Clause { neg: false, pred: Pred::Kinds(vec![k]) });
        }
        for tok in tokenize(q) {
            let (neg, t) = match tok.strip_prefix('!').or_else(|| tok.strip_prefix('-')) {
                Some(rest) if !rest.is_empty() => (true, rest.to_string()),
                _ => (false, tok.clone()),
            };
            let pred = if let Some((k, v)) = t.split_once(':').filter(|(k, _)| {
                matches!(
                    k.to_ascii_lowercase().as_str(),
                    "ext" | "type" | "kind" | "size" | "dm" | "modified" | "path" | "in" | "re"
                        | "regex" | "label" | "tag" | "hidden" | "file" | "folder" | "dir"
                )
            }) {
                match k.to_ascii_lowercase().as_str() {
                    "ext" => Pred::Ext(alts(v.trim_start_matches('.'))),
                    "type" | "kind" => {
                        let ks: Vec<Kind> = v.split(['|', ',']).filter_map(Kind::parse).collect();
                        if ks.is_empty() {
                            continue;
                        }
                        Pred::Kinds(ks)
                    }
                    "size" => match parse_range(v, parse_size, 0, u64::MAX) {
                        Some((a, b)) => Pred::Size(a, b),
                        None => match parse_size(v) {
                            Some(s) => Pred::Size(s, u64::MAX),
                            None => continue,
                        },
                    },
                    "dm" | "modified" => match parse_dm(v) {
                        Some((a, b)) => Pred::Mtime(a, b),
                        None => continue,
                    },
                    "path" | "in" => Pred::Path(alts(v)),
                    "re" | "regex" => match regex::bytes::RegexBuilder::new(v).case_insensitive(true).build() {
                        Ok(r) => Pred::Regex(r, None),
                        Err(_) => continue,
                    },
                    "label" | "tag" => {
                        let l = v.to_ascii_lowercase();
                        if !neg {
                            out.labels.push(l.clone());
                        }
                        Pred::Label(l)
                    }
                    "hidden" => Pred::Hidden(matches!(v, "yes" | "true" | "1" | "")),
                    "file" => Pred::OnlyFiles,
                    "folder" | "dir" => Pred::OnlyDirs,
                    _ => continue,
                }
            } else if t.contains('*') || t.contains('?') {
                // `*.ext` is just an extension filter.
                let simple_ext = t.strip_prefix("*.").filter(|e| !e.is_empty() && !e.contains(['*', '?', '.']));
                if let Some(e) = simple_ext {
                    Pred::Ext(alts(e))
                } else {
                    match glob_to_regex(&t) {
                        Some(r) => {
                            let lit = t
                                .split(['*', '?'])
                                .max_by_key(|p| p.len())
                                .filter(|p| !p.is_empty())
                                .map(|p| p.to_ascii_lowercase().into_bytes());
                            Pred::Regex(r, lit)
                        }
                        None => continue,
                    }
                }
            } else if t.contains('/') || t.contains('\\') {
                let norm = t.replace(['/', '\\'], std::path::MAIN_SEPARATOR_STR);
                Pred::Path(alts(&norm))
            } else {
                if !neg {
                    for a in t.split('|') {
                        if !a.is_empty() {
                            out.highlight.push(a.to_string());
                        }
                    }
                }
                Pred::Name(alts(&t))
            };
            out.clauses.push(Clause { neg, pred });
        }
        // Cheap predicates first.
        out.clauses.sort_by_key(|c| match c.pred {
            Pred::Kinds(_) | Pred::OnlyDirs | Pred::OnlyFiles | Pred::Hidden(_) | Pred::Size(..) | Pred::Mtime(..) => 0,
            Pred::Ext(_) => 1,
            Pred::Name(_) => 2,
            Pred::Regex(..) => 3,
            Pred::Path(_) => 4,
            Pred::Label(_) => 5,
        });
        out
    }
}

/// Media label lookup: resolves `label:` predicates.
pub trait LabelSource: Sync {
    /// Lower-case full paths of files carrying a label that contains `label`.
    fn paths_with_label(&self, label: &str) -> HashSet<String>;
}

pub struct SearchOutput {
    pub ids: Vec<u32>,
    pub elapsed_us: u64,
}

struct Ctx<'a> {
    idx: &'a Index,
    label_sets: Vec<(String, HashSet<String>)>,
    name_finders: Vec<Vec<memmem::Finder<'a>>>,
}

pub fn run(idx: &Index, q: &Query, sort: SortKey, desc: bool, labels: &dyn LabelSource) -> SearchOutput {
    let t = Instant::now();
    let label_sets: Vec<(String, HashSet<String>)> =
        q.labels.iter().map(|l| (l.clone(), labels.paths_with_label(l))).collect();
    let name_finders = q
        .clauses
        .iter()
        .map(|c| match &c.pred {
            Pred::Name(a) | Pred::Path(a) => a.iter().map(|s| memmem::Finder::new(s.as_slice())).collect(),
            _ => Vec::new(),
        })
        .collect();
    let ctx = Ctx { idx, label_sets, name_finders };

    const CHUNK: usize = 16 * 1024;
    let n = idx.len();
    let mut ids: Vec<u32> = (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .flat_map_iter(|c| {
            let lo = c * CHUNK;
            let hi = ((c + 1) * CHUNK).min(n);
            let mut out = Vec::new();
            let mut pathbuf = Vec::with_capacity(256);
            let mut cached_parent = NONE;
            for i in lo..hi {
                if matches(&ctx, q, i as u32, &mut pathbuf, &mut cached_parent) {
                    out.push(i as u32);
                }
            }
            out
        })
        .collect();

    sort_ids(idx, &mut ids, q, sort, desc);
    SearchOutput { ids, elapsed_us: t.elapsed().as_micros() as u64 }
}

#[inline]
fn matches(ctx: &Ctx, q: &Query, id: u32, pathbuf: &mut Vec<u8>, cached_parent: &mut u32) -> bool {
    let idx = ctx.idx;
    let e = &idx.entries[id as usize];
    if e.is_deleted() || e.parent == NONE {
        return false;
    }
    let name = idx.lower_name(id);
    for (ci, c) in q.clauses.iter().enumerate() {
        let hit = match &c.pred {
            Pred::Name(_) => ctx.name_finders[ci].iter().any(|f| f.find(name).is_some()),
            Pred::Path(_) => {
                if *cached_parent != e.parent {
                    idx.write_lower_dir_path(e.parent, pathbuf);
                    *cached_parent = e.parent;
                }
                let dir_len = pathbuf.len();
                if pathbuf.last() != Some(&(std::path::MAIN_SEPARATOR as u8)) {
                    pathbuf.push(std::path::MAIN_SEPARATOR as u8);
                }
                pathbuf.extend_from_slice(name);
                let r = ctx.name_finders[ci].iter().any(|f| f.find(pathbuf).is_some());
                pathbuf.truncate(dir_len);
                r
            }
            Pred::Regex(r, lit) => {
                lit.as_ref().is_none_or(|l| memmem::find(name, l).is_some())
                    && r.is_match(&idx.names[e.name_off as usize..e.name_off as usize + e.name_len as usize])
            }
            Pred::Ext(exts) => match memchr::memrchr(b'.', name) {
                Some(p) if !e.is_dir() => exts.iter().any(|x| &name[p + 1..] == x.as_slice()),
                _ => false,
            },
            Pred::Kinds(ks) => ks.iter().any(|k| *k as u8 == e.kind),
            Pred::Size(a, b) => !e.is_dir() && e.size >= *a && e.size <= *b,
            Pred::Mtime(a, b) => e.mtime >= *a && e.mtime <= *b,
            Pred::Hidden(h) => (e.flags & F_HIDDEN != 0) == *h,
            Pred::OnlyFiles => !e.is_dir(),
            Pred::OnlyDirs => e.is_dir(),
            Pred::Label(l) => {
                if !e.kind().is_media() {
                    false
                } else {
                    let set = ctx.label_sets.iter().find(|(k, _)| k == l).map(|(_, s)| s);
                    match set {
                        Some(s) if !s.is_empty() => s.contains(&idx.path_string(id).to_lowercase()),
                        _ => false,
                    }
                }
            }
        };
        if hit == c.neg {
            return false;
        }
    }
    true
}

fn relevance(idx: &Index, id: u32, terms: &[Vec<u8>]) -> u32 {
    let name = idx.lower_name(id);
    let mut score = 0u32;
    for t in terms {
        if name == t.as_slice() {
            score += 0;
        } else if name.starts_with(t) {
            score += 1;
        } else if let Some(p) = memmem::find(name, t) {
            // Word-boundary matches beat mid-word matches.
            let b = name[p - 1];
            score += if b.is_ascii_alphanumeric() { 3 } else { 2 };
        } else {
            score += 4;
        }
    }
    // Prefer shorter names and folders slightly; keep within u32 range.
    score * 4096 + (name.len().min(4095) as u32)
}

pub fn sort_ids(idx: &Index, ids: &mut [u32], q: &Query, sort: SortKey, desc: bool) {
    let cmp_name = |a: &u32, b: &u32| idx.lower_name(*a).cmp(idx.lower_name(*b));
    match sort {
        SortKey::Relevance => {
            if q.highlight.is_empty() {
                return;
            }
            let terms: Vec<Vec<u8>> = q.highlight.iter().map(|s| s.to_ascii_lowercase().into_bytes()).collect();
            // Score once, then sort by key.
            let mut keyed: Vec<(u32, u32)> = ids.par_iter().map(|&i| (relevance(idx, i, &terms), i)).collect();
            keyed.par_sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| cmp_name(&a.1, &b.1)));
            for (dst, (_, i)) in ids.iter_mut().zip(keyed) {
                *dst = i;
            }
        }
        SortKey::Name => ids.par_sort_unstable_by(cmp_name),
        SortKey::Size => ids.par_sort_unstable_by_key(|&i| std::cmp::Reverse(idx.entries[i as usize].size)),
        SortKey::Modified => ids.par_sort_unstable_by_key(|&i| std::cmp::Reverse(idx.entries[i as usize].mtime)),
        SortKey::Kind => ids.par_sort_unstable_by(|a, b| {
            idx.entries[*a as usize].kind.cmp(&idx.entries[*b as usize].kind).then_with(|| cmp_name(a, b))
        }),
        SortKey::Path => {
            let mut keyed: Vec<(String, u32)> = ids.par_iter().map(|&i| (idx.path_string(i).to_lowercase(), i)).collect();
            keyed.par_sort_unstable();
            for (dst, (_, i)) in ids.iter_mut().zip(keyed) {
                *dst = i;
            }
        }
    }
    if desc {
        ids.reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, ScanProgress};

    struct NoLabels;
    impl LabelSource for NoLabels {
        fn paths_with_label(&self, _: &str) -> HashSet<String> {
            HashSet::new()
        }
    }

    #[test]
    fn query_semantics() {
        let dir = std::env::temp_dir().join(format!("insight-q-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj/src")).unwrap();
        std::fs::write(dir.join("proj/src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(dir.join("proj/README.md"), b"# hi").unwrap();
        std::fs::write(dir.join("Holiday Photo.jpg"), vec![1u8; 4096]).unwrap();
        let idx = Index::scan(&[dir.to_string_lossy().into()], &[], &ScanProgress::default());
        let names = |q: &str| -> Vec<String> {
            let out = run(&idx, &Query::parse(q, None), SortKey::Name, false, &NoLabels);
            out.ids.iter().map(|&i| idx.name(i).to_string()).collect()
        };
        assert_eq!(names("main"), vec!["main.rs"]);
        assert_eq!(names("ext:rs,md"), vec!["main.rs", "README.md"]);
        assert_eq!(names("type:image"), vec!["Holiday Photo.jpg"]);
        assert_eq!(names("\"holiday photo\""), vec!["Holiday Photo.jpg"]);
        assert_eq!(names("size:>1kb"), vec!["Holiday Photo.jpg"]);
        assert_eq!(names("src/main"), vec!["main.rs"]);
        assert_eq!(names("*.md"), vec!["README.md"]);
        assert_eq!(names("folder: !proj"), vec!["src"]);
        assert_eq!(names("readme|main !md"), vec!["main.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
