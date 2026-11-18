//! Cleanup & optimisation toolbox, modelled on Mole (`mo clean`, `mo optimize`,
//! `mo uninstall`, `mo purge`) and CleanMyMac.
//!
//! * [`clean`]   — caches, logs, trash, browser & developer caches, app leftovers.
//! * [`optimize`]— maintenance tasks (DNS, Quick Look, Launch Services, Spotlight…)
//!                 and login / background items.
//! * [`apps`]    — uninstall apps together with their leftovers.
//! * [`purge`]   — rebuildable project artifacts (node_modules, target, .build…).

// Several helpers are only used on some platforms.
#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

pub mod apps;
pub mod clean;
pub mod optimize;
pub mod purge;

use rayon::prelude::*;
use std::path::Path;

/// Allocated size and file count of a path (does not follow symlinks).
pub fn du(p: &Path) -> (u64, u64) {
    let Ok(md) = std::fs::symlink_metadata(p) else { return (0, 0) };
    if md.is_dir() {
        let Ok(rd) = std::fs::read_dir(p) else { return (0, 0) };
        let kids: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        kids.par_iter().map(|k| du(k)).reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    } else {
        (alloc_size(&md), 1)
    }
}

#[cfg(unix)]
fn alloc_size(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    (md.blocks() * 512).min(md.len().max(md.blocks() * 512))
}

#[cfg(not(unix))]
fn alloc_size(md: &std::fs::Metadata) -> u64 {
    md.len()
}

/// Permanently delete a file or directory tree. Returns bytes freed (best effort).
pub fn remove(p: &Path) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(p)?;
    if md.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

pub fn home() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"))
}

/// Run a shell command line, optionally elevated (macOS admin prompt / pkexec / UAC).
pub fn run_shell(cmd: &str, admin: bool) -> anyhow::Result<String> {
    let out = if cfg!(target_os = "macos") {
        if admin {
            let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            std::process::Command::new("osascript").args(["-e", &format!("do shell script \"{escaped}\" with administrator privileges")]).output()?
        } else {
            std::process::Command::new("/bin/sh").args(["-c", cmd]).output()?
        }
    } else if cfg!(windows) {
        if admin {
            let ps = format!("Start-Process cmd -ArgumentList '/C {}' -Verb RunAs -Wait", cmd.replace('\'', "''"));
            std::process::Command::new("powershell").args(["-NoProfile", "-Command", &ps]).output()?
        } else {
            std::process::Command::new("cmd").args(["/C", cmd]).output()?
        }
    } else if admin {
        std::process::Command::new("pkexec").args(["sh", "-c", cmd]).output()?
    } else {
        std::process::Command::new("sh").args(["-c", cmd]).output()?
    };
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    if out.status.success() {
        Ok(text(&out.stdout))
    } else {
        let e = text(&out.stderr);
        if e.contains("User canceled") || e.contains("-128") {
            anyhow::bail!("cancelled")
        }
        anyhow::bail!("{}", if e.is_empty() { text(&out.stdout) } else { e })
    }
}
