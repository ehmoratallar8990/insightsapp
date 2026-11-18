//! `mo uninstall`-style app removal: the app plus the support files it left
//! around the system. Caches, logs and saved state are pre-selected; personal data
//! (settings, Application Support, containers) is listed but kept unless ticked.

use super::{du, home};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct App {
    pub name: String,
    pub path: String,
    pub bundle_id: String,
    pub version: String,
    pub bytes: u64,
    /// How to remove it when it is not a plain bundle (Windows / Linux packages).
    pub uninstall_cmd: Option<String>,
    pub source: &'static str,
}

#[derive(Clone, Debug)]
pub struct Leftover {
    pub path: String,
    pub bytes: u64,
    pub kind: &'static str,
    /// Personal data (settings, documents) — unticked by default.
    pub personal: bool,
    pub on: bool,
}

/// Installed apps, biggest first.
pub fn list() -> Vec<App> {
    let mut v = platform_list();
    v.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
    v
}

#[cfg(target_os = "macos")]
fn app_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/Applications"), PathBuf::from("/Applications/Utilities"), home().join("Applications")]
}

#[cfg(target_os = "macos")]
fn bundles() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in app_dirs() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "app") {
                out.push(p);
            } else if p.is_dir() && p.file_name().is_some_and(|n| n != "Utilities") {
                // Vendor folders such as "/Applications/Adobe Photoshop 2025/".
                if let Ok(rd2) = std::fs::read_dir(&p) {
                    out.extend(rd2.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "app")));
                }
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn info(p: &Path) -> (String, String) {
    let v = plist::Value::from_file(p.join("Contents/Info.plist")).ok();
    let d = v.as_ref().and_then(|v| v.as_dictionary());
    let get = |k: &str| d.and_then(|d| d.get(k)).and_then(|x| x.as_string()).unwrap_or_default().to_string();
    let ver = get("CFBundleShortVersionString");
    (get("CFBundleIdentifier"), ver)
}

/// Lower-case bundle ids of every installed app (incl. Apple's).
#[cfg(target_os = "macos")]
pub fn installed_bundle_ids() -> HashSet<String> {
    let mut v: Vec<PathBuf> = bundles();
    for d in ["/System/Applications", "/System/Applications/Utilities"] {
        if let Ok(rd) = std::fs::read_dir(d) {
            v.extend(rd.flatten().map(|e| e.path()));
        }
    }
    v.par_iter().map(|p| info(p).0.to_ascii_lowercase()).filter(|s| !s.is_empty()).collect()
}

#[cfg(target_os = "macos")]
fn platform_list() -> Vec<App> {
    bundles()
        .par_iter()
        .filter_map(|p| {
            let (bundle_id, version) = info(p);
            if bundle_id.starts_with("com.apple.") {
                return None; // System apps can't be removed (SIP).
            }
            let name = p.file_stem()?.to_string_lossy().into_owned();
            Some(App { name, path: p.to_string_lossy().into_owned(), bundle_id, version, bytes: du(p).0, uninstall_cmd: None, source: "App" })
        })
        .collect()
}

#[cfg(windows)]
fn platform_list() -> Vec<App> {
    let ps = r#"$k='HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*','HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*','HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*';
Get-ItemProperty $k -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -and $_.UninstallString -and -not $_.SystemComponent } |
Select-Object DisplayName,DisplayVersion,EstimatedSize,UninstallString,QuietUninstallString,InstallLocation,PSChildName | ConvertTo-Json -Compress"#;
    let Ok(o) = std::process::Command::new("powershell").args(["-NoProfile", "-Command", ps]).output() else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) else { return Vec::new() };
    let arr = match v {
        serde_json::Value::Array(a) => a,
        o @ serde_json::Value::Object(_) => vec![o],
        _ => Vec::new(),
    };
    let mut seen = HashSet::new();
    arr.into_iter()
        .filter_map(|a| {
            let name = a["DisplayName"].as_str()?.to_string();
            if !seen.insert(name.clone()) {
                return None;
            }
            let cmd = a["QuietUninstallString"].as_str().or(a["UninstallString"].as_str())?.to_string();
            Some(App {
                name,
                path: a["InstallLocation"].as_str().unwrap_or_default().to_string(),
                bundle_id: a["PSChildName"].as_str().unwrap_or_default().to_string(),
                version: a["DisplayVersion"].as_str().unwrap_or_default().to_string(),
                bytes: a["EstimatedSize"].as_u64().unwrap_or(0) * 1024,
                uninstall_cmd: Some(cmd),
                source: "Installed program",
            })
        })
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_list() -> Vec<App> {
    let mut v = Vec::new();
    // Flatpak
    if let Ok(o) = std::process::Command::new("flatpak").args(["list", "--app", "--columns=application,name,version,size"]).output() {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            let c: Vec<&str> = l.split('\t').collect();
            if c.len() >= 2 {
                v.push(App {
                    name: c[1].to_string(),
                    path: String::new(),
                    bundle_id: c[0].to_string(),
                    version: c.get(2).unwrap_or(&"").to_string(),
                    bytes: c.get(3).and_then(|s| parse_human(s)).unwrap_or(0),
                    uninstall_cmd: Some(format!("flatpak uninstall -y {}", c[0])),
                    source: "Flatpak",
                });
            }
        }
    }
    // Snap
    if let Ok(o) = std::process::Command::new("snap").arg("list").output() {
        for l in String::from_utf8_lossy(&o.stdout).lines().skip(1) {
            let c: Vec<&str> = l.split_whitespace().collect();
            if c.len() >= 2 && !["core", "core18", "core20", "core22", "core24", "snapd", "bare"].contains(&c[0]) && !c[0].starts_with("gnome-") && !c[0].starts_with("gtk-") {
                let size = du(&PathBuf::from(format!("/snap/{}", c[0]))).0;
                v.push(App {
                    name: c[0].to_string(),
                    path: format!("/snap/{}", c[0]),
                    bundle_id: c[0].to_string(),
                    version: c[1].to_string(),
                    bytes: size,
                    uninstall_cmd: Some(format!("pkexec snap remove {}", c[0])),
                    source: "Snap",
                });
            }
        }
    }
    // AppImages in the usual places
    for d in [home().join("Applications"), home().join(".local/bin"), home().join("Downloads")] {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("appimage")) {
                    v.push(App {
                        name: p.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
                        path: p.to_string_lossy().into_owned(),
                        bundle_id: String::new(),
                        version: String::new(),
                        bytes: du(&p).0,
                        uninstall_cmd: None,
                        source: "AppImage",
                    });
                }
            }
        }
    }
    v
}

#[cfg(all(unix, not(target_os = "macos")))]
fn parse_human(s: &str) -> Option<u64> {
    let s = s.trim().replace('\u{a0}', " ");
    let (num, unit) = s.split_once(' ')?;
    let n: f64 = num.replace(',', ".").parse().ok()?;
    let m = match unit.trim().to_ascii_lowercase().as_str() {
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        _ => 1.0,
    };
    Some((n * m) as u64)
}

/// Support files belonging to `app`.
pub fn leftovers(app: &App) -> Vec<Leftover> {
    let mut out: Vec<(PathBuf, &'static str, bool)> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let lib = home().join("Library");
        let id = app.bundle_id.to_ascii_lowercase();
        let name = app.name.to_ascii_lowercase();
        // (folder, label, personal, match-by-name-too)
        let places: [(&str, &'static str, bool, bool); 12] = [
            ("Caches", "Cache", false, true),
            ("Logs", "Logs", false, true),
            ("Saved Application State", "Saved state", false, false),
            ("HTTPStorages", "Web storage", false, false),
            ("WebKit", "Web data", false, false),
            ("Cookies", "Cookies", false, false),
            ("LaunchAgents", "Login item", false, false),
            ("Application Scripts", "Scripts", false, false),
            ("Preferences", "Settings", true, false),
            ("Application Support", "App data", true, true),
            ("Containers", "Sandbox data", true, false),
            ("Group Containers", "Shared data", true, false),
        ];
        for (dir, label, personal, by_name) in places {
            let Ok(rd) = std::fs::read_dir(lib.join(dir)) else { continue };
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                let stem = n.trim_end_matches(".plist").trim_end_matches(".savedstate").trim_end_matches(".binarycookies");
                let by_id = !id.is_empty() && (stem == id || stem.starts_with(&format!("{id}.")) || (dir == "Group Containers" && stem.ends_with(&format!(".{id}"))));
                let by_nm = by_name && name.len() >= 3 && stem == name;
                if by_id || by_nm {
                    out.push((e.path(), label, personal));
                }
            }
        }
        if !id.is_empty() {
            for d in ["/Library/LaunchAgents", "/Library/LaunchDaemons", "/Library/PrivilegedHelperTools"] {
                if let Ok(rd) = std::fs::read_dir(d) {
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().to_ascii_lowercase().starts_with(&id) {
                            out.push((e.path(), "Helper (system)", false));
                        }
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        let base = app.name.to_ascii_lowercase();
        for root in [dirs::data_dir(), dirs::data_local_dir()].into_iter().flatten() {
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().to_ascii_lowercase() == base {
                        out.push((e.path(), "App data", true));
                    }
                }
            }
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if app.source == "Flatpak" {
            out.push((home().join(".var/app").join(&app.bundle_id), "App data", true));
        }
        let n = app.name.to_ascii_lowercase();
        for root in [dirs::config_dir(), dirs::cache_dir(), dirs::data_dir()].into_iter().flatten() {
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().to_ascii_lowercase() == n {
                        out.push((e.path(), "App data", true));
                    }
                }
            }
        }
    }
    let mut seen = HashSet::new();
    out.retain(|(p, _, _)| seen.insert(p.clone()) && p.exists());
    out.par_iter()
        .map(|(p, kind, personal)| {
            // Sandboxed containers belong to the app; reading inside triggers a privacy prompt.
            let s = p.to_string_lossy();
            let bytes = if s.contains("/Containers/") { 0 } else { du(p).0 };
            Leftover { path: s.into_owned(), bytes, kind, personal: *personal, on: !*personal }
        })
        .collect()
}

/// Is any process of this app running?
pub fn is_running(app: &App) -> bool {
    if app.path.is_empty() {
        return false;
    }
    let Ok(mut sys) = sysinfo::System::new() else { return false };
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::OnlyIfNotSet),
    );
    let prefix = Path::new(&app.path);
    sys.processes().values().any(|p| p.exe().is_some_and(|e| e.starts_with(prefix)))
}

/// Remove the app and the ticked leftovers. Bundles and files go to the Trash;
/// package-managed apps run their uninstaller.
pub fn uninstall(app: &App, leftovers: &[Leftover]) -> anyhow::Result<(u64, Vec<String>)> {
    let mut freed = 0u64;
    let mut failed = Vec::new();
    if let Some(cmd) = &app.uninstall_cmd {
        super::run_shell(cmd, false)?;
        freed += app.bytes;
    } else {
        trash::delete(&app.path).map_err(|e| anyhow::anyhow!("{}: {e}", app.name))?;
        freed += app.bytes;
    }
    for l in leftovers.iter().filter(|l| l.on) {
        match trash::delete(&l.path) {
            Ok(()) => freed += l.bytes,
            Err(e) => failed.push(format!("{}: {e}", l.path)),
        }
    }
    Ok((freed, failed))
}
