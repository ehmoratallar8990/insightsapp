//! `mo optimize`-style maintenance tasks plus login / background items.

use super::{home, run_shell};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Task {
    pub id: &'static str,
    pub icon: &'static str,
    pub title: &'static str,
    pub detail: &'static str,
    /// Asks for an administrator password.
    pub admin: bool,
    /// Part of "Run recommended".
    pub recommended: bool,
    cmd: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskState {
    Idle,
    Running,
    Done(String),
    Failed(String),
}

pub fn tasks() -> Vec<Task> {
    let mut v = Vec::new();
    let mut t = |id, icon, title, detail, admin, recommended, cmd| v.push(Task { id, icon, title, detail, admin, recommended, cmd });
    #[cfg(target_os = "macos")]
    {
        t("dns", "🌐", "Flush DNS cache", "Fixes sites that won't load after network or DNS changes.", true, true, "dscacheutil -flushcache; killall -HUP mDNSResponder");
        t("ram", "💻", "Free up RAM", "Releases inactive memory so busy apps get room (purge).", true, false, "/usr/sbin/purge");
        t(
            "quicklook",
            "👁",
            "Rebuild Quick Look thumbnails",
            "Fixes stale or missing previews in Finder.",
            false,
            true,
            "qlmanage -r cache >/dev/null 2>&1; qlmanage -r >/dev/null 2>&1; echo ok",
        );
        t(
            "launchservices",
            "🔗",
            "Rebuild Launch Services",
            "Removes duplicate apps from \"Open With\" and fixes wrong default apps.",
            false,
            true,
            "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -kill -r -domain local -domain system -domain user && echo ok",
        );
        t("fonts", "🔤", "Reset font caches", "Fixes garbled or missing fonts.", false, false, "atsutil databases -removeUser && echo ok");
        t("spotlight", "🔦", "Reindex Spotlight", "Rebuilds the Spotlight index when searches miss files. Takes a while in the background.", true, false, "mdutil -E /");
        t("finder", "📂", "Restart Finder & Dock", "Unsticks a frozen Finder, Dock or menu bar.", false, false, "killall Finder; killall Dock; killall SystemUIServer; echo ok");
        t("verify", "🛠", "Verify startup disk", "Read-only check of the file system (diskutil verifyVolume).", false, false, "diskutil verifyVolume / 2>&1 | tail -n 2");
    }
    #[cfg(target_os = "linux")]
    {
        t("dns", "🌐", "Flush DNS cache", "Fixes sites that won't load after network changes.", false, true, "resolvectl flush-caches 2>/dev/null || systemd-resolve --flush-caches; echo ok");
        t("ram", "💻", "Drop file caches", "Releases cached memory (sync; drop_caches).", true, false, "sync; echo 3 > /proc/sys/vm/drop_caches");
        t("thumbs", "🖼", "Rebuild thumbnails", "Clears stale file-manager previews.", false, true, "rm -rf \"$HOME/.cache/thumbnails\"; echo ok");
        t("journal", "📜", "Trim system journal", "Keeps only the last 7 days of systemd logs.", true, true, "journalctl --vacuum-time=7d");
        t("fonts", "🔤", "Rebuild font cache", "Fixes missing fonts.", false, false, "fc-cache -f && echo ok");
        t("desktop", "🔗", "Refresh app menu", "Rebuilds the desktop-entry database.", false, false, "update-desktop-database \"$HOME/.local/share/applications\" 2>/dev/null; echo ok");
    }
    #[cfg(windows)]
    {
        t("dns", "🌐", "Flush DNS cache", "Fixes sites that won't load after network changes.", false, true, "ipconfig /flushdns");
        t("explorer", "📂", "Restart Explorer", "Unsticks a frozen taskbar or Explorer.", false, false, "taskkill /f /im explorer.exe & start explorer.exe");
        t("icons", "🖼", "Rebuild icon cache", "Fixes blank or wrong icons.", false, true, "ie4uinit.exe -show");
        t("cleanmgr", "✨", "Windows Disk Cleanup", "Opens the built-in cleaner for system files.", false, false, "start cleanmgr");
        t("storagesense", "💾", "Storage Sense", "Opens Windows' automatic cleanup settings.", false, false, "start ms-settings:storagesense");
        t("sfc", "🛠", "Verify system files", "sfc /verifyonly — read-only integrity check.", true, false, "sfc /verifyonly");
    }
    v
}

pub fn run(task: &Task) -> TaskState {
    match run_shell(task.cmd, task.admin) {
        Ok(out) => {
            let line = out.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            TaskState::Done(if line.is_empty() || line == "ok" { "Done".into() } else { line.chars().take(120).collect() })
        }
        Err(e) => TaskState::Failed(e.to_string().chars().take(160).collect()),
    }
}

/// Something that starts automatically at login / boot.
#[derive(Clone, Debug)]
pub struct StartupItem {
    pub name: String,
    pub path: String,
    pub detail: String,
    /// Can be removed without admin rights.
    pub user: bool,
}

pub fn startup_items() -> Vec<StartupItem> {
    let mut v = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let dirs_: [(PathBuf, bool, &str); 3] = [
            (home().join("Library/LaunchAgents"), true, "Login agent (you)"),
            (PathBuf::from("/Library/LaunchAgents"), false, "Login agent (all users)"),
            (PathBuf::from("/Library/LaunchDaemons"), false, "Background daemon"),
        ];
        for (d, user, kind) in dirs_ {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "plist") {
                    continue;
                }
                let file = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let program = plist::Value::from_file(&p).ok().and_then(|v| {
                    let d = v.as_dictionary()?.clone();
                    d.get("Program")
                        .and_then(|x| x.as_string().map(String::from))
                        .or_else(|| d.get("ProgramArguments")?.as_array()?.first()?.as_string().map(String::from))
                });
                let name = program
                    .as_deref()
                    .and_then(|pr| app_name_from_path(pr))
                    .unwrap_or_else(|| file.clone());
                v.push(StartupItem { name, path: p.to_string_lossy().into_owned(), detail: format!("{kind} · {file}"), user });
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let d = dirs::config_dir().unwrap_or_else(|| home().join(".config")).join("autostart");
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                let name = text.lines().find_map(|l| l.strip_prefix("Name=")).unwrap_or_default().to_string();
                let exec = text.lines().find_map(|l| l.strip_prefix("Exec=")).unwrap_or_default().to_string();
                v.push(StartupItem {
                    name: if name.is_empty() { p.file_stem().unwrap_or_default().to_string_lossy().into_owned() } else { name },
                    path: p.to_string_lossy().into_owned(),
                    detail: exec,
                    user: true,
                });
            }
        }
    }
    #[cfg(windows)]
    {
        if let Some(d) = dirs::data_dir().map(|d| d.join("Microsoft/Windows/Start Menu/Programs/Startup")) {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.file_name().is_some_and(|n| n == "desktop.ini") {
                        continue;
                    }
                    v.push(StartupItem {
                        name: p.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
                        path: p.to_string_lossy().into_owned(),
                        detail: "Startup folder".into(),
                        user: true,
                    });
                }
            }
        }
    }
    v.sort_by(|a, b| b.user.cmp(&a.user).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    v
}

/// "/Applications/Foo.app/Contents/MacOS/foo" → "Foo".
pub fn app_name_from_path(p: &str) -> Option<String> {
    let i = p.find(".app/")?;
    let head = &p[..i];
    Some(head.rsplit('/').next()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names() {
        assert_eq!(app_name_from_path("/Applications/Docker.app/Contents/MacOS/com.docker.backend").as_deref(), Some("Docker"));
        assert_eq!(app_name_from_path("/usr/local/bin/thing"), None);
        assert!(!tasks().is_empty());
    }
}
