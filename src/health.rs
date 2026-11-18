//! Live system health: CPU, memory, disk, battery, network, connected devices and a
//! 0–100 health score (in the spirit of CleanMyMac's menu panel and Mole's `mo status`).
//!
//! A background thread samples once per second while the Health page is visible and
//! idles otherwise; slow probes (hardware model, Bluetooth) run on their own thread.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const HISTORY: usize = 60;

#[derive(Clone, Default)]
pub struct Battery {
    pub percent: f32,
    pub charging: bool,
    pub plugged: bool,
    /// Minutes until empty (discharging) or full (charging).
    pub minutes: Option<u32>,
    pub health: Option<f32>,
    pub cycles: Option<u32>,
    pub temp_c: Option<f32>,
}

#[derive(Clone)]
pub struct Volume {
    pub name: String,
    pub mount: String,
    pub total: u64,
    pub available: u64,
}

#[derive(Clone)]
pub struct Peripheral {
    pub name: String,
    pub kind: String,
    pub battery: Option<u8>,
}

#[derive(Clone)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
}

#[derive(Clone, PartialEq)]
pub enum SpeedTest {
    Idle,
    Running(f64),
    Done { down_mbps: f64, latency_ms: f64 },
    Failed(String),
}

#[derive(Clone)]
pub struct Snapshot {
    pub ready: bool,
    pub model: String,
    pub chip: String,
    pub os: String,
    pub host: String,
    pub uptime: u64,
    pub cpu: f32,
    pub cores: Vec<f32>,
    pub load: (f64, f64, f64),
    pub cpu_temp: Option<f32>,
    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_pressure: f32,
    pub swap_used: u64,
    pub disk_name: String,
    pub disk_total: u64,
    pub disk_available: u64,
    pub battery: Option<Battery>,
    pub net_down: f64,
    pub net_up: f64,
    pub volumes: Vec<Volume>,
    pub peripherals: Vec<Peripheral>,
    pub top_cpu: Vec<Proc>,
    pub top_mem: Vec<Proc>,
    pub cpu_hist: VecDeque<f32>,
    pub mem_hist: VecDeque<f32>,
    pub down_hist: VecDeque<f32>,
    pub up_hist: VecDeque<f32>,
    pub speed: SpeedTest,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            ready: false,
            model: default_model(),
            chip: String::new(),
            os: String::new(),
            host: String::new(),
            uptime: 0,
            cpu: 0.0,
            cores: Vec::new(),
            load: (0.0, 0.0, 0.0),
            cpu_temp: None,
            mem_total: 0,
            mem_used: 0,
            mem_pressure: 0.0,
            swap_used: 0,
            disk_name: String::new(),
            disk_total: 0,
            disk_available: 0,
            battery: None,
            net_down: 0.0,
            net_up: 0.0,
            volumes: Vec::new(),
            peripherals: Vec::new(),
            top_cpu: Vec::new(),
            top_mem: Vec::new(),
            cpu_hist: VecDeque::new(),
            mem_hist: VecDeque::new(),
            down_hist: VecDeque::new(),
            up_hist: VecDeque::new(),
            speed: SpeedTest::Idle,
        }
    }
}

fn default_model() -> String {
    if cfg!(target_os = "macos") {
        "Mac".into()
    } else if cfg!(windows) {
        "PC".into()
    } else {
        "Computer".into()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    Excellent,
    Good,
    Fair,
    Poor,
}

impl Grade {
    pub fn label(self) -> &'static str {
        match self {
            Grade::Excellent => "Excellent",
            Grade::Good => "Good",
            Grade::Fair => "Fair",
            Grade::Poor => "Poor",
        }
    }
}

/// A reason the score dropped, shown under the headline.
pub struct Issue {
    pub penalty: u8,
    pub text: String,
}

impl Snapshot {
    /// Mole-style health score: start at 100 and subtract for each pressure point.
    pub fn score(&self) -> (u8, Grade, Vec<Issue>) {
        let mut issues = Vec::new();
        let mut add = |p: u8, t: String| issues.push(Issue { penalty: p, text: t });
        if self.disk_total > 0 {
            let free = self.disk_available as f64 / self.disk_total as f64;
            if free < 0.05 {
                add(30, format!("Startup disk almost full ({:.0}% free)", free * 100.0));
            } else if free < 0.10 {
                add(20, format!("Startup disk low on space ({:.0}% free)", free * 100.0));
            } else if free < 0.20 {
                add(8, format!("Startup disk {:.0}% free", free * 100.0));
            }
        }
        if self.mem_pressure > 80.0 {
            add(25, format!("High memory pressure ({:.0}%)", self.mem_pressure));
        } else if self.mem_pressure > 60.0 {
            add(10, format!("Elevated memory pressure ({:.0}%)", self.mem_pressure));
        }
        let cpu = self.cpu_hist.iter().rev().take(10).sum::<f32>() / self.cpu_hist.len().clamp(1, 10) as f32;
        if cpu > 85.0 {
            add(20, format!("Sustained CPU load ({cpu:.0}%)"));
        } else if cpu > 60.0 {
            add(8, format!("Busy CPU ({cpu:.0}%)"));
        }
        if let Some(t) = self.cpu_temp {
            if t > 95.0 {
                add(15, format!("Running hot ({t:.0} °C)"));
            } else if t > 85.0 {
                add(5, format!("Warm ({t:.0} °C)"));
            }
        }
        if let Some(h) = self.battery.as_ref().and_then(|b| b.health) {
            if h < 80.0 {
                add(10, format!("Battery health {h:.0}% — service recommended"));
            }
        }
        if self.swap_used > 4 << 30 {
            add(5, format!("Heavy swap use ({})", crate::storage::human(self.swap_used)));
        }
        if self.uptime > 14 * 86_400 {
            add(5, format!("Up for {} days — a restart clears leaks", self.uptime / 86_400));
        }
        let total: u32 = issues.iter().map(|i| i.penalty as u32).sum();
        let s = 100u32.saturating_sub(total) as u8;
        let g = match s {
            85..=100 => Grade::Excellent,
            70..85 => Grade::Good,
            50..70 => Grade::Fair,
            _ => Grade::Poor,
        };
        issues.sort_by_key(|i| std::cmp::Reverse(i.penalty));
        (s, g, issues)
    }
}

pub struct Monitor {
    pub snap: Arc<Mutex<Snapshot>>,
    pub active: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Monitor {
    pub fn start(ctx: egui::Context) -> Self {
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let active = Arc::new(AtomicBool::new(false));
        {
            let (snap, active, ctx) = (snap.clone(), active.clone(), ctx.clone());
            std::thread::Builder::new().name("health".into()).spawn(move || sampler(snap, active, ctx)).ok();
        }
        {
            let (snap, active) = (snap.clone(), active.clone());
            std::thread::Builder::new().name("health-slow".into()).spawn(move || slow_probes(snap, active)).ok();
        }
        Monitor { snap, active, ctx }
    }

    pub fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
    }

    pub fn get(&self) -> Snapshot {
        self.snap.lock().clone()
    }

    /// Measure download throughput against Cloudflare's public speed endpoint.
    pub fn speed_test(&self) {
        let snap = self.snap.clone();
        let ctx = self.ctx.clone();
        snap.lock().speed = SpeedTest::Running(0.0);
        std::thread::spawn(move || {
            let r = run_speed_test(&snap, &ctx);
            snap.lock().speed = match r {
                Ok((down_mbps, latency_ms)) => SpeedTest::Done { down_mbps, latency_ms },
                Err(e) => SpeedTest::Failed(e.to_string()),
            };
            ctx.request_repaint();
        });
    }
}

#[cfg(feature = "download")]
fn run_speed_test(snap: &Arc<Mutex<Snapshot>>, ctx: &egui::Context) -> anyhow::Result<(f64, f64)> {
    use std::io::Read;
    let t0 = Instant::now();
    ureq::get("https://speed.cloudflare.com/__down?bytes=0").call()?;
    let latency = t0.elapsed().as_secs_f64() * 1000.0;
    let total: u64 = 50_000_000;
    let resp = ureq::get(&format!("https://speed.cloudflare.com/__down?bytes={total}")).call()?;
    let mut r = resp.into_body().into_reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    let t = Instant::now();
    let mut last = Instant::now();
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        got += n as u64;
        if last.elapsed() > Duration::from_millis(150) {
            let mbps = got as f64 * 8.0 / t.elapsed().as_secs_f64() / 1e6;
            snap.lock().speed = SpeedTest::Running(mbps);
            ctx.request_repaint();
            last = Instant::now();
        }
        if t.elapsed() > Duration::from_secs(12) {
            break;
        }
    }
    Ok((got as f64 * 8.0 / t.elapsed().as_secs_f64() / 1e6, latency))
}

#[cfg(not(feature = "download"))]
fn run_speed_test(_: &Arc<Mutex<Snapshot>>, _: &egui::Context) -> anyhow::Result<(f64, f64)> {
    anyhow::bail!("built without network support")
}

fn push(h: &mut VecDeque<f32>, v: f32) {
    if h.len() >= HISTORY {
        h.pop_front();
    }
    h.push_back(v);
}

fn sampler(snap: Arc<Mutex<Snapshot>>, active: Arc<AtomicBool>, ctx: egui::Context) {
    use sysinfo::{Components, Networks, ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(mut sys) = System::new() else { return };
    let mut nets = Networks::new_with_refreshed_list().ok();
    let mut comps = Components::new_with_refreshed_list().ok();
    let batt = starship_battery::Manager::new().ok();
    let mut batteries: Vec<starship_battery::Battery> =
        batt.as_ref().and_then(|m| m.batteries().ok()).map(|b| b.flatten().collect()).unwrap_or_default();
    {
        let mut s = snap.lock();
        s.os = System::long_os_version().unwrap_or_default();
        s.host = System::host_name().unwrap_or_default();
        sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        s.chip = sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
    }
    let mut last = Instant::now();
    let mut tick = 0u64;
    loop {
        if !active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let dt = last.elapsed().as_secs_f64().max(0.2);
        last = Instant::now();
        let (mut down, mut up) = (0u64, 0u64);
        if let Some(n) = nets.as_mut() {
            n.refresh(true);
            for (name, d) in n.iter() {
                let name = name.to_string_lossy();
                if is_virtual_iface(&name) {
                    continue;
                }
                down += d.received();
                up += d.transmitted();
            }
        }
        let temp = comps.as_mut().and_then(|c| {
            c.refresh(false);
            let temps: Vec<f32> = c
                .iter()
                .filter(|c| {
                    let l = c.label().to_ascii_lowercase();
                    l.contains("cpu") || l.contains("pmu") || l.contains("core") || l.contains("package") || l.contains("soc") || l.contains("tdie") || l.contains("tctl")
                })
                .filter_map(|c| c.temperature())
                .filter(|t| *t > 5.0 && *t < 130.0)
                .collect();
            if temps.is_empty() { None } else { Some(temps.iter().cloned().fold(0.0, f32::max)) }
        });
        // Processes are comparatively expensive; refresh every 3 s.
        let procs = if tick % 3 == 0 {
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cpu().with_memory());
            let mut v: Vec<Proc> = sys
                .processes()
                .values()
                .map(|p| Proc { pid: p.pid().as_u32(), name: p.name().to_string_lossy().into_owned(), cpu: p.cpu_usage(), mem: p.memory() })
                .collect();
            let n_cpu = sys.cpus().len().max(1) as f32;
            for p in v.iter_mut() {
                p.cpu /= n_cpu;
            }
            v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
            let top_cpu: Vec<Proc> = v.iter().take(6).cloned().collect();
            v.sort_by(|a, b| b.mem.cmp(&a.mem));
            let top_mem: Vec<Proc> = v.into_iter().take(6).collect();
            Some((top_cpu, top_mem))
        } else {
            None
        };
        let battery = batteries.first_mut().and_then(|b| {
            batt.as_ref()?.refresh(b).ok()?;
            Some(read_battery(b))
        });
        let disk = root_disk();
        let volumes = if tick % 5 == 0 { Some(external_volumes()) } else { None };

        let total = sys.total_memory();
        let used = sys.used_memory();
        let pressure = memory_pressure().unwrap_or_else(|| if total > 0 { (total - sys.available_memory().min(total)) as f32 / total as f32 * 100.0 } else { 0.0 });
        {
            let mut s = snap.lock();
            s.ready = true;
            s.uptime = System::uptime().unwrap_or(0);
            s.cpu = sys.global_cpu_usage();
            s.cores = sys.cpus().iter().map(|c| c.usage()).collect();
            if let Ok(l) = System::load_average() {
                s.load = (l.one, l.five, l.fifteen);
            }
            s.cpu_temp = temp;
            s.mem_total = total;
            s.mem_used = used;
            s.mem_pressure = pressure;
            s.swap_used = sys.used_swap();
            if let Some((n, t, a)) = disk {
                s.disk_name = n;
                s.disk_total = t;
                s.disk_available = a;
            }
            s.battery = battery;
            if tick > 0 {
                s.net_down = down as f64 / dt;
                s.net_up = up as f64 / dt;
            }
            if let Some((c, m)) = procs {
                s.top_cpu = c;
                s.top_mem = m;
            }
            if let Some(v) = volumes {
                s.volumes = v;
            }
            let (cpu, down, up) = (s.cpu, s.net_down as f32, s.net_up as f32);
            push(&mut s.cpu_hist, cpu);
            push(&mut s.mem_hist, pressure);
            push(&mut s.down_hist, down);
            push(&mut s.up_hist, up);
        }
        ctx.request_repaint();
        tick += 1;
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn read_battery(b: &starship_battery::Battery) -> Battery {
    use starship_battery::State;
    use starship_battery::units::{ratio::percent, thermodynamic_temperature::degree_celsius, time::minute};
    let state = b.state();
    let minutes = match state {
        State::Charging => b.time_to_full(),
        State::Discharging => b.time_to_empty(),
        _ => None,
    }
    .map(|t| t.get::<minute>() as u32)
    .filter(|m| *m > 0 && *m < 24 * 60);
    let health = b.state_of_health().get::<percent>();
    Battery {
        percent: b.state_of_charge().get::<percent>(),
        charging: state == State::Charging,
        plugged: matches!(state, State::Charging | State::Full),
        minutes,
        health: if health > 1.0 && health <= 110.0 { Some(health.min(100.0)) } else { None },
        cycles: b.cycle_count(),
        temp_c: b.temperature().map(|t| t.get::<degree_celsius>()),
    }
}

fn is_virtual_iface(n: &str) -> bool {
    let n = n.to_ascii_lowercase();
    ["lo", "utun", "awdl", "llw", "bridge", "anpi", "ap", "gif", "stf", "docker", "veth", "virbr", "vmnet", "tailscale", "zt"]
        .iter()
        .any(|p| n.starts_with(p))
        || n.contains("loopback")
        || n.contains("virtual")
}

fn root_disk() -> Option<(String, u64, u64)> {
    let disks = sysinfo::Disks::new_with_refreshed_list().ok()?;
    let root = if cfg!(windows) { std::env::var("SystemDrive").map(|d| format!("{d}\\")).unwrap_or_else(|_| "C:\\".into()) } else { "/".into() };
    let d = disks.iter().find(|d| d.mount_point().to_string_lossy().eq_ignore_ascii_case(&root)).or_else(|| disks.iter().next())?;
    let mut name = d.name().to_string_lossy().into_owned();
    if name.is_empty() || name.starts_with("/dev") {
        name = if cfg!(windows) { format!("Local Disk ({})", root.trim_end_matches('\\')) } else { "System disk".into() };
    }
    Some((name, d.total_space(), d.available_space()))
}

pub fn external_volumes() -> Vec<Volume> {
    let Ok(disks) = sysinfo::Disks::new_with_refreshed_list() else { return Vec::new() };
    let mut out: Vec<Volume> = disks
        .iter()
        .filter(|d| {
            let m = d.mount_point().to_string_lossy();
            let ext = if cfg!(target_os = "macos") {
                m.starts_with("/Volumes/") && !m.starts_with("/Volumes/Recovery") && !m.contains("com.apple")
            } else if cfg!(windows) {
                d.is_removable()
            } else {
                m.starts_with("/media/") || m.starts_with("/run/media/") || m.starts_with("/mnt/")
            };
            ext && d.total_space() > 0
        })
        .map(|d| {
            let mount = d.mount_point().to_string_lossy().into_owned();
            let mut name = d.name().to_string_lossy().into_owned();
            if name.is_empty() || name.starts_with("/dev") {
                name = std::path::Path::new(&mount).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(mount.clone());
            }
            Volume { name, mount, total: d.total_space(), available: d.available_space() }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.mount == b.mount);
    out
}

/// Eject / unmount an external volume.
pub fn eject(mount: &str) -> anyhow::Result<()> {
    let st = if cfg!(target_os = "macos") {
        std::process::Command::new("diskutil").arg("eject").arg(mount).output()?
    } else if cfg!(windows) {
        let drive = mount.trim_end_matches('\\');
        let ps = format!("(New-Object -comObject Shell.Application).Namespace(17).ParseName('{drive}').InvokeVerb('Eject')");
        std::process::Command::new("powershell").args(["-NoProfile", "-Command", &ps]).output()?
    } else {
        std::process::Command::new("gio").args(["mount", "-u", mount]).output().or_else(|_| std::process::Command::new("umount").arg(mount).output())?
    };
    if st.status.success() { Ok(()) } else { anyhow::bail!("{}", String::from_utf8_lossy(&st.stderr).trim()) }
}

/// macOS: `kern.memorystatus_level` is the percentage of memory "free" from the
/// kernel's point of view; pressure is its complement (what Activity Monitor graphs).
#[cfg(target_os = "macos")]
fn memory_pressure() -> Option<f32> {
    let mut level: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let name = c"kern.memorystatus_level";
    let r = unsafe { libc::sysctlbyname(name.as_ptr(), &mut level as *mut _ as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0) };
    if r == 0 && (0..=100).contains(&level) { Some((100 - level) as f32) } else { None }
}

#[cfg(not(target_os = "macos"))]
fn memory_pressure() -> Option<f32> {
    None
}

/// Slow, rarely-changing data: hardware model and Bluetooth peripherals.
fn slow_probes(snap: Arc<Mutex<Snapshot>>, active: Arc<AtomicBool>) {
    let (model, chip) = hardware_model();
    {
        let mut s = snap.lock();
        if !model.is_empty() {
            s.model = model;
        }
        if !chip.is_empty() {
            s.chip = chip;
        }
    }
    let mut last: Option<Instant> = None;
    loop {
        if active.load(Ordering::Relaxed) && last.is_none_or(|t| t.elapsed() > Duration::from_secs(20)) {
            let p = peripherals();
            snap.lock().peripherals = p;
            last = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

fn hardware_model() -> (String, String) {
    #[cfg(target_os = "macos")]
    {
        if let Ok(o) = std::process::Command::new("system_profiler").args(["SPHardwareDataType", "-json"]).output() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                let h = &v["SPHardwareDataType"][0];
                let model = h["machine_name"].as_str().unwrap_or_default().to_string();
                let chip = h["chip_type"].as_str().or(h["cpu_type"].as_str()).unwrap_or_default().to_string();
                return (model, chip);
            }
        }
        (String::new(), String::new())
    }
    #[cfg(target_os = "linux")]
    {
        let read = |p: &str| std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default();
        let vendor = read("/sys/devices/virtual/dmi/id/sys_vendor");
        let product = read("/sys/devices/virtual/dmi/id/product_name");
        let model = format!("{vendor} {product}").trim().to_string();
        (model, String::new())
    }
    #[cfg(windows)]
    {
        let model = sysinfo::Product::name().unwrap_or_default();
        (model, String::new())
    }
}

fn peripherals() -> Vec<Peripheral> {
    #[cfg(target_os = "macos")]
    {
        let Ok(o) = std::process::Command::new("system_profiler").args(["SPBluetoothDataType", "-json"]).output() else { return Vec::new() };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) else { return Vec::new() };
        parse_bluetooth(&v)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// Parse `system_profiler SPBluetoothDataType -json` connected devices.
pub fn parse_bluetooth(v: &serde_json::Value) -> Vec<Peripheral> {
    let mut out = Vec::new();
    let Some(list) = v["SPBluetoothDataType"][0]["device_connected"].as_array() else { return out };
    for item in list {
        let Some(obj) = item.as_object() else { continue };
        for (name, d) in obj {
            let pct = |k: &str| d[k].as_str().and_then(|s| s.trim_end_matches('%').trim().parse::<u8>().ok());
            let parts: Vec<u8> = ["device_batteryLevelLeft", "device_batteryLevelRight"].iter().filter_map(|k| pct(k)).collect();
            let battery = if !parts.is_empty() { parts.iter().min().copied() } else { pct("device_batteryLevelMain").or(pct("device_batteryLevel")) };
            let kind = d["device_minorType"].as_str().unwrap_or("Device").to_string();
            out.push(Peripheral { name: name.clone(), kind, battery });
        }
    }
    out
}

/// Ask the OS to release cached / inactive memory. Needs an administrator password.
pub fn free_memory() -> anyhow::Result<()> {
    let out = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript").args(["-e", "do shell script \"/usr/sbin/purge\" with administrator privileges"]).output()?
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("pkexec").args(["sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"]).output()?
    } else {
        anyhow::bail!("Windows manages standby memory itself — quit heavy apps below instead")
    };
    if out.status.success() { Ok(()) } else { anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// Politely ask a process to quit (SIGTERM / taskkill).
pub fn quit_process(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
    }
    #[cfg(windows)]
    {
        std::process::Command::new("taskkill").args(["/PID", &pid.to_string()]).status().map(|s| s.success()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluetooth_parse() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"SPBluetoothDataType":[{"device_connected":[
                {"AirPods Pro":{"device_batteryLevelLeft":"74 %","device_batteryLevelRight":"80%","device_minorType":"Headphones"}},
                {"Keyboard":{"device_batteryLevelMain":"84%","device_minorType":"Keyboard"}},
                {"Speaker":{"device_minorType":"Speaker"}}]}]}"#,
        )
        .unwrap();
        let p = parse_bluetooth(&v);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].battery, Some(74));
        assert_eq!(p[1].battery, Some(84));
        assert_eq!(p[2].battery, None);
    }

    #[test]
    fn score_drops_on_full_disk() {
        let s = Snapshot { disk_total: 100, disk_available: 2, ..Default::default() };
        let (score, grade, issues) = s.score();
        assert_eq!(score, 70);
        assert!(grade == Grade::Good);
        assert_eq!(issues.len(), 1);
    }
}
