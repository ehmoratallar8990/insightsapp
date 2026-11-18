//! Native "choose a folder" dialog without extra dependencies: AppleScript on macOS,
//! the .NET FolderBrowserDialog on Windows, zenity / kdialog on Linux. Runs on a
//! background thread (the dialogs block) and hands the result back to the UI.

use super::cleanup_view::Job;

#[derive(Default)]
pub struct FolderPicker {
    job: Job<Option<String>>,
    /// Which field asked (so one picker can serve several fields).
    pub target: u8,
}

impl FolderPicker {
    pub fn open(&mut self, ctx: &egui::Context, target: u8, title: &str, start: &str) {
        if self.job.running() {
            return;
        }
        self.target = target;
        let (title, start) = (title.to_string(), start.to_string());
        self.job.start(ctx, move || pick_folder(&title, &start));
    }

    pub fn busy(&self) -> bool {
        self.job.running()
    }

    /// The chosen folder, once, for the field that asked.
    pub fn poll(&mut self) -> Option<(u8, String)> {
        self.job.poll().flatten().map(|p| (self.target, p))
    }
}

fn existing_dir(start: &str) -> Option<std::path::PathBuf> {
    std::path::Path::new(start.trim()).ancestors().find(|p| p.is_dir()).map(|p| p.to_path_buf())
}

pub fn pick_folder(title: &str, start: &str) -> Option<String> {
    let start = existing_dir(start).or_else(dirs::home_dir).unwrap_or_default();
    let s = start.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let t = title.replace('"', "'");
    #[cfg(target_os = "macos")]
    let out = std::process::Command::new("osascript")
        .args(["-e", &format!("POSIX path of (choose folder with prompt \"{t}\" default location POSIX file \"{s}\")")])
        .output()
        .ok()?;
    #[cfg(windows)]
    let out = {
        let ps = format!(
            "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description = '{}'; $d.SelectedPath = '{}'; if ($d.ShowDialog() -eq 'OK') {{ $d.SelectedPath }}",
            t.replace('\'', "''"),
            start.to_string_lossy().replace('\'', "''")
        );
        std::process::Command::new("powershell").args(["-NoProfile", "-STA", "-Command", &ps]).output().ok()?
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let out = std::process::Command::new("zenity")
        .args(["--file-selection", "--directory", &format!("--title={t}"), &format!("--filename={}/", start.display())])
        .output()
        .or_else(|_| std::process::Command::new("kdialog").args(["--getexistingdirectory", &start.to_string_lossy(), "--title", &t]).output())
        .ok()?;
    let _ = s;
    if !out.status.success() {
        return None; // cancelled
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().trim_end_matches('/').to_string();
    if p.is_empty() { None } else { Some(p) }
}
