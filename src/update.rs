//! Automatic updates from GitHub Releases.
//!
//! 1. Check `releases/latest` (at launch, then every 6 h).
//! 2. If newer, download this platform's asset in the background and verify it
//!    against the release's `SHA256SUMS.txt` — no checksum, no install.
//! 3. Stage it next to the app's data; swap it in when the user clicks
//!    "Restart to update" or, failing that, automatically when Insight quits.
//!
//! The swap is an atomic rename of the bundle / executable, so a failed update
//! leaves the current version in place.

#![cfg_attr(not(feature = "download"), allow(dead_code, unused_imports))]

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const REPO: &str = "ehmoratallar8990/insightsapp";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub url: String,
    pub asset: String,
    pub asset_url: String,
    pub sums_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(Release, f32),
    /// Downloaded, verified and extracted; `PathBuf` is the staged bundle / binary.
    Ready(Release, PathBuf),
    Installing,
    Failed(String),
}

pub struct Updater {
    pub state: Arc<Mutex<State>>,
    pub last_check: Arc<Mutex<Option<Instant>>>,
    ctx: egui::Context,
}

pub fn asset_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Insight-macOS-universal.zip"
    } else if cfg!(windows) {
        "Insight-Windows-x86_64.zip"
    } else {
        "Insight-Linux-x86_64.tar.gz"
    }
}

fn stage_dir() -> PathBuf {
    crate::index::data_dir().join("update")
}

pub fn is_newer(remote: &str, current: &str) -> bool {
    let p = |s: &str| semver::Version::parse(s.trim_start_matches('v')).ok();
    match (p(remote), p(current)) {
        (Some(r), Some(c)) => r > c,
        _ => false,
    }
}

impl Updater {
    pub fn new(ctx: egui::Context) -> Self {
        // A previous Windows update leaves the old exe behind; tidy up.
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::fs::remove_file(exe.with_extension("old"));
        }
        Updater { state: Arc::new(Mutex::new(State::Idle)), last_check: Arc::new(Mutex::new(None)), ctx }
    }

    pub fn get(&self) -> State {
        self.state.lock().clone()
    }

    /// Call every frame; kicks off periodic checks when `auto` is on.
    pub fn tick(&self, auto: bool, uptime: f64) {
        if !auto || uptime < 8.0 {
            return;
        }
        let due = self.last_check.lock().is_none_or(|t| t.elapsed() > CHECK_EVERY);
        let idle = matches!(*self.state.lock(), State::Idle | State::UpToDate | State::Failed(_));
        if due && idle {
            self.check(true);
        }
    }

    /// Check for a new release; with `download`, fetch it right away.
    pub fn check(&self, download: bool) {
        *self.last_check.lock() = Some(Instant::now());
        *self.state.lock() = State::Checking;
        let (state, ctx) = (self.state.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let r = latest();
            let next = match r {
                Ok(Some(rel)) => {
                    if download {
                        *state.lock() = State::Downloading(rel.clone(), 0.0);
                        ctx.request_repaint();
                        match fetch_and_stage(&rel, &state, &ctx) {
                            Ok(p) => State::Ready(rel, p),
                            Err(e) => State::Failed(format!("{e:#}")),
                        }
                    } else {
                        State::Available(rel)
                    }
                }
                Ok(None) => State::UpToDate,
                Err(e) => State::Failed(format!("{e:#}")),
            };
            *state.lock() = next;
            ctx.request_repaint();
        });
    }

    pub fn download(&self) {
        let State::Available(rel) = self.get() else { return };
        *self.state.lock() = State::Downloading(rel.clone(), 0.0);
        let (state, ctx) = (self.state.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let next = match fetch_and_stage(&rel, &state, &ctx) {
                Ok(p) => State::Ready(rel, p),
                Err(e) => State::Failed(format!("{e:#}")),
            };
            *state.lock() = next;
            ctx.request_repaint();
        });
    }

    /// Swap in the staged build. With `relaunch`, start the new version and quit.
    pub fn apply(&self, relaunch: bool) -> Result<()> {
        let State::Ready(_, staged) = self.get() else { bail!("no update is ready") };
        *self.state.lock() = State::Installing;
        match install(&staged) {
            Ok(target) => {
                if relaunch {
                    relaunch_from(&target);
                    self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Ok(())
            }
            Err(e) => {
                *self.state.lock() = State::Failed(format!("{e:#}"));
                Err(e)
            }
        }
    }
}

#[cfg(feature = "download")]
fn http_get(url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    Ok(ureq::get(url)
        .header("User-Agent", "insight-updater")
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("GET {url}"))?)
}

/// The latest release if it is newer than this build.
#[cfg(feature = "download")]
pub fn latest() -> Result<Option<Release>> {
    let mut resp = http_get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))?;
    let v: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("no tag in release"))?;
    if !is_newer(tag, CURRENT) {
        return Ok(None);
    }
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let find = |name: &str| assets.iter().find(|a| a["name"].as_str() == Some(name)).and_then(|a| a["browser_download_url"].as_str()).map(String::from);
    let asset_url = find(asset_name()).ok_or_else(|| anyhow!("release {tag} has no {} asset", asset_name()))?;
    Ok(Some(Release {
        version: tag.trim_start_matches('v').to_string(),
        notes: v["body"].as_str().unwrap_or_default().to_string(),
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        asset: asset_name().to_string(),
        asset_url,
        sums_url: find("SHA256SUMS.txt"),
    }))
}

#[cfg(not(feature = "download"))]
pub fn latest() -> Result<Option<Release>> {
    bail!("this build has no network support (built without the `download` feature)")
}

#[cfg(feature = "download")]
fn fetch_and_stage(rel: &Release, state: &Arc<Mutex<State>>, ctx: &egui::Context) -> Result<PathBuf> {
    use sha2::Digest;
    use std::io::{Read, Write};
    let sums_url = rel.sums_url.as_ref().ok_or_else(|| anyhow!("release has no SHA256SUMS.txt — refusing to install unverified code"))?;
    let sums = http_get(sums_url)?.body_mut().read_to_string()?;
    let expected = sums
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            let h = it.next()?;
            let f = it.next()?.trim_start_matches('*');
            (f == rel.asset).then(|| h.to_ascii_lowercase())
        })
        .ok_or_else(|| anyhow!("{} missing from SHA256SUMS.txt", rel.asset))?;

    let dir = stage_dir().join(&rel.version);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let archive = dir.join(&rel.asset);
    let resp = http_get(&rel.asset_url)?;
    let total = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let mut r = resp.into_body().into_reader();
    let mut f = std::io::BufWriter::new(std::fs::File::create(&archive)?);
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        f.write_all(&buf[..n])?;
        done += n as u64;
        if total > 0 {
            *state.lock() = State::Downloading(rel.clone(), done as f32 / total as f32);
            ctx.request_repaint();
        }
    }
    f.flush()?;
    drop(f);
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != expected {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("checksum mismatch for {} — download discarded", rel.asset);
    }
    extract(&archive, &dir)
}

#[cfg(not(feature = "download"))]
fn fetch_and_stage(_: &Release, _: &Arc<Mutex<State>>, _: &egui::Context) -> Result<PathBuf> {
    bail!("no network support")
}

/// Unpack the archive; returns the staged bundle (macOS) or executable.
fn extract(archive: &Path, dir: &Path) -> Result<PathBuf> {
    let out = dir.join("x");
    std::fs::create_dir_all(&out)?;
    let ok = if cfg!(target_os = "macos") {
        std::process::Command::new("ditto").arg("-x").arg("-k").arg(archive).arg(&out).status()?
    } else if cfg!(windows) {
        let ps = format!("Expand-Archive -Force -LiteralPath '{}' -DestinationPath '{}'", archive.display(), out.display());
        std::process::Command::new("powershell").args(["-NoProfile", "-Command", &ps]).status()?
    } else {
        std::process::Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&out).status()?
    };
    if !ok.success() {
        bail!("could not unpack {}", archive.display());
    }
    let staged = if cfg!(target_os = "macos") {
        out.join("Insight.app")
    } else if cfg!(windows) {
        out.join("Insight.exe")
    } else {
        out.join("insight/insight")
    };
    if !staged.exists() {
        bail!("unexpected archive layout (missing {})", staged.display());
    }
    Ok(staged)
}

/// The installed thing to replace: the `.app` bundle on macOS, else the executable.
pub fn install_target() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if cfg!(target_os = "macos") {
        let app = exe.ancestors().find(|p| p.extension().is_some_and(|x| x == "app")).map(Path::to_path_buf);
        app.ok_or_else(|| anyhow!("not running from an app bundle (development build?) — install from the release instead"))
    } else {
        if exe.components().any(|c| c.as_os_str() == "target") && exe.to_string_lossy().contains("release") {
            bail!("development build — updates apply to installed copies only");
        }
        Ok(exe)
    }
}

/// Atomically replace the installed copy with `staged`. Returns the install path.
fn install(staged: &Path) -> Result<PathBuf> {
    let target = install_target()?;
    let parent = target.parent().ok_or_else(|| anyhow!("bad install path"))?;
    let backup = parent.join(format!(
        ".{}.old-{}",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let incoming = parent.join(format!(".{}.new", target.file_name().unwrap_or_default().to_string_lossy()));
    let _ = remove_any(&incoming);
    // Copy (staging may be on another volume), then two renames on the same volume.
    copy_tree(staged, &incoming).with_context(|| format!("no write access to {}", parent.display()))?;
    #[cfg(unix)]
    if !cfg!(target_os = "macos") {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&target, &backup).context("could not move the current version aside")?;
    if let Err(e) = std::fs::rename(&incoming, &target) {
        let _ = std::fs::rename(&backup, &target);
        return Err(e).context("could not move the new version into place");
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr").args(["-dr", "com.apple.quarantine"]).arg(&target).status();
    }
    // Windows can't delete a running exe; Updater::new removes `.old` next launch.
    if cfg!(windows) {
        let _ = std::fs::rename(&backup, target.with_extension("old"));
    } else {
        let _ = remove_any(&backup);
    }
    let _ = std::fs::remove_dir_all(stage_dir());
    Ok(target)
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) => Err(e),
    }
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        // ditto preserves symlinks, permissions and code signatures inside bundles.
        let st = std::process::Command::new("ditto").arg(src).arg(dst).status()?;
        if !st.success() {
            bail!("ditto failed");
        }
        return Ok(());
    }
    let md = std::fs::symlink_metadata(src)?;
    if md.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn relaunch_from(target: &Path) {
    if cfg!(target_os = "macos") {
        let script = format!("sleep 1; /usr/bin/open -n '{}'", target.display().to_string().replace('\'', "'\\''"));
        let _ = std::process::Command::new("/bin/sh").args(["-c", &script]).spawn();
    } else {
        let _ = std::process::Command::new(target).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
