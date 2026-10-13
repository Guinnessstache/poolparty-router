//! Platform helpers: finding llama-server, models and the GPU; opening the
//! app window and folders; starting with Windows.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Windows: don't flash a console window for child processes.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where settings, logs and handoff briefs live by default.
pub fn app_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        PathBuf::from(appdata).join("PoolParty Router")
    } else {
        home().join(".config").join("poolparty-router")
    }
}

// ------------------------------------------------------------ discovery

pub fn find_llama_servers() -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    let mut found = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            found.push(dir.join(exe));
        }
    }
    let h = home();
    for p in [
        h.join(".unsloth/llama.cpp/build/bin/Release").join(exe),
        h.join(".unsloth/llama.cpp/build/bin").join(exe),
        h.join("llama.cpp/build/bin/Release").join(exe),
        PathBuf::from("C:/llama").join(exe),
        PathBuf::from("/opt/homebrew/bin").join(exe),
    ] {
        found.push(p);
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for p in found {
        if p.is_file() && !out.iter().any(|o| same_file(o, &p)) {
            out.push(p);
        }
    }
    out
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelFile {
    pub name: String,
    pub path: String,
    pub size_gb: f64,
    /// Fits in the GPU with ~1.5 GB left for context, when the GPU is known.
    pub fits_gpu: Option<bool>,
}

pub fn model_dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    let h = home();
    let mut dirs = vec![
        h.join(".lmstudio/models"),
        h.join(".cache/lm-studio/models"),
        h.join(".cache/huggingface/hub"),
        h.join("models"),
        h.join("Downloads"),
        h.join("Documents/models"),
    ];
    dirs.extend(extra.iter().cloned());
    dirs.into_iter().filter(|d| d.is_dir()).collect()
}

/// GGUF files over 300 MB in the usual places, largest first. Vision
/// projector files (mmproj) are skipped: they aren't models on their own.
pub fn find_models(extra: &[PathBuf], gpu_gb: Option<f64>) -> Vec<ModelFile> {
    let mut out: Vec<ModelFile> = Vec::new();
    for dir in model_dirs(extra) {
        walk(&dir, 0, &mut |p, size| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            let lower = name.to_lowercase();
            if !lower.ends_with(".gguf") || lower.contains("mmproj") || size < 300_000_000 {
                return;
            }
            // Split models: only offer the first part; llama-server finds the rest.
            if lower.contains("-of-") && !lower.contains("00001-of-") {
                return;
            }
            let size_gb = size as f64 / 1e9;
            if out.iter().any(|m| m.name == name && (m.size_gb - size_gb).abs() < 0.01) {
                return;
            }
            out.push(ModelFile {
                name: name.trim_end_matches(".gguf").to_string(),
                path: p.display().to_string(),
                size_gb: (size_gb * 10.0).round() / 10.0,
                fits_gpu: gpu_gb.map(|g| size_gb + 1.5 <= g),
            });
        });
    }
    out.sort_by(|a, b| b.size_gb.partial_cmp(&a.size_gb).unwrap());
    out
}

fn walk(dir: &Path, depth: u32, f: &mut dyn FnMut(&Path, u64)) {
    if depth > 7 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && depth > 0 || name == "node_modules" || name == "blobs" {
            continue;
        }
        // metadata() follows links, so Hugging Face's snapshot links count.
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_dir() {
            walk(&p, depth + 1, f);
        } else {
            f(&p, md.len());
        }
    }
}

// ------------------------------------------------------------ GPU

#[derive(Debug, Clone, Default, Serialize)]
pub struct Gpu {
    pub name: String,
    pub memory_total_gb: f64,
    pub memory_used_gb: f64,
    pub utilization: u32,
    pub temperature_c: Option<u32>,
}

/// NVIDIA GPUs via nvidia-smi. None on machines without one.
pub fn gpu() -> Option<Gpu> {
    let out = quiet(Command::new("nvidia-smi").args([
        "--query-gpu=name,memory.total,memory.used,utilization.gpu,temperature.gpu",
        "--format=csv,noheader,nounits",
    ]))
    .output()
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.to_string();
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    if f.len() < 4 {
        return None;
    }
    Some(Gpu {
        name: f[0].trim_start_matches("NVIDIA ").to_string(),
        memory_total_gb: f[1].parse::<f64>().ok()? / 1024.0,
        memory_used_gb: f[2].parse::<f64>().unwrap_or(0.0) / 1024.0,
        utilization: f[3].parse().unwrap_or(0),
        temperature_c: f.get(4).and_then(|t| t.parse().ok()),
    })
}

// ------------------------------------------------------------ desktop

/// Open the app UI in its own window (Edge or Chrome "app" mode: no tabs or
/// address bar), falling back to the default browser.
pub fn open_window(url: &str) {
    #[cfg(windows)]
    {
        let profile = app_dir().join("window");
        let candidates = [
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ];
        for exe in candidates {
            if Path::new(exe).is_file() {
                let ok = Command::new(exe)
                    .arg(format!("--app={url}"))
                    .arg(format!("--user-data-dir={}", profile.display()))
                    .args(["--window-size=1280,880", "--no-first-run", "--no-default-browser-check"])
                    .spawn()
                    .is_ok();
                if ok {
                    return;
                }
            }
        }
        let _ = quiet(Command::new("cmd").args(["/c", "start", "", url])).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

/// Show a folder (or a file inside its folder) in Explorer / Finder.
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        if path.is_file() {
            let _ = Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
        } else {
            let _ = Command::new("explorer").arg(path).spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = if path.is_file() { path.parent().unwrap_or(path) } else { path };
        let _ = Command::new("xdg-open").arg(dir).spawn();
    }
}

/// Windows: put "PoolParty Router" in the Start menu the first time the app
/// runs, pointing at wherever the .exe was unzipped.
pub fn ensure_start_menu_shortcut() {
    #[cfg(windows)]
    {
        let Some(appdata) = std::env::var_os("APPDATA") else { return };
        let lnk = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\PoolParty Router.lnk");
        let Ok(exe) = std::env::current_exe() else { return };
        // Don't pin a copy that's running from a temp or download preview folder.
        let s = exe.display().to_string().to_lowercase();
        if lnk.exists() || s.contains("\\temp\\") || s.contains("\\temporary internet files\\") {
            return;
        }
        let dir = exe.parent().map(|d| d.display().to_string()).unwrap_or_default();
        let ps = format!(
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath='{}'; \
             $s.WorkingDirectory='{}'; $s.IconLocation='{},0'; $s.Description='PoolParty Router'; $s.Save()",
            lnk.display().to_string().replace('\'', "''"),
            exe.display().to_string().replace('\'', "''"),
            dir.replace('\'', "''"),
            exe.display().to_string().replace('\'', "''"),
        );
        let _ = quiet(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &ps])).status();
    }
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "PoolParty Router";

/// Start with Windows: None where it isn't supported.
pub fn autostart_enabled() -> Option<bool> {
    if !cfg!(windows) {
        return None;
    }
    let out = quiet(Command::new("reg").args(["query", RUN_KEY, "/v", RUN_NAME])).output().ok()?;
    Some(out.status.success())
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("starting with the computer is only supported on Windows".into());
    }
    let status = if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let value = format!("\"{}\" --background", exe.display());
        quiet(Command::new("reg").args(["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &value, "/f"]))
            .status()
    } else {
        quiet(Command::new("reg").args(["delete", RUN_KEY, "/v", RUN_NAME, "/f"])).status()
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("reg.exe exited with {s}")),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_models_and_skips_projectors() {
        let dir = std::env::temp_dir().join(format!("pp-models-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("org/repo")).unwrap();
        let big = |p: &Path| {
            let f = std::fs::File::create(p).unwrap();
            f.set_len(400_000_000).unwrap(); // sparse: no real disk use
        };
        big(&dir.join("org/repo/Coder-7B-Q4_K_M.gguf"));
        big(&dir.join("org/repo/mmproj-F16.gguf"));
        big(&dir.join("org/repo/Big-00002-of-00002.gguf"));
        big(&dir.join("org/repo/Big-00001-of-00002.gguf"));
        std::fs::write(dir.join("tiny.gguf"), b"x").unwrap();
        let found = find_models(&[dir.clone()], Some(12.0));
        let names: Vec<_> = found.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Coder-7B-Q4_K_M"), "{names:?}");
        assert!(names.contains(&"Big-00001-of-00002"));
        assert!(!names.iter().any(|n| n.contains("mmproj") || n.contains("00002-of") || *n == "tiny"));
        assert_eq!(found[0].fits_gpu, Some(true));
        std::fs::remove_dir_all(dir).ok();
    }
}
