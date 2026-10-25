use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Progress plumbing for the GUI (`ibackup ui`). The library reports progress
// with indicatif bars/spinners and eprintln! notes. While the GUI window is up
// (`set_ui_mode(true)`), notes are also queued for the window and every active
// bar is tracked so the UI can snapshot those values each frame.
// ---------------------------------------------------------------------------

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
static UI_MODE: AtomicBool = AtomicBool::new(false);

/// Called by the GUI just before opening the window, and again at exit.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn set_ui_mode(on: bool) {
    UI_MODE.store(on, Ordering::SeqCst);
}

pub fn ui_mode() -> bool {
    UI_MODE.load(Ordering::SeqCst)
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
static UI_LOG: LazyLock<Mutex<VecDeque<String>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Print `line` to stderr as the CLI always did; with the GUI running, also
/// queue it for the window's log panel.
pub fn say(line: &str) {
    eprintln!("{line}");
    #[cfg(feature = "gui")]
    say_only(line);
}

/// Queue a line for the GUI log panel only (no stderr copy).
#[cfg(feature = "gui")]
pub fn say_only(line: &str) {
    if !ui_mode() {
        return;
    }
    let mut log = UI_LOG.lock().unwrap_or_else(|p| p.into_inner());
    log.push_back(line.to_string());
    while log.len() > 400 {
        log.pop_front();
    }
}

/// Take and clear the lines queued for the GUI (called by the UI each frame).
#[cfg(feature = "gui")]
pub fn ui_take_log() -> Vec<String> {
    if !ui_mode() {
        return Vec::new();
    }
    let mut log = UI_LOG.lock().unwrap_or_else(|p| p.into_inner());
    log.drain(..).collect()
}

#[derive(Clone, Copy, PartialEq)]
pub enum GaugeUnit {
    /// position / total are byte counts (media copies)
    Bytes,
    /// position / total are 0..10_000 like the device-backup progress protocol
    Percent,
    /// no known total; show message only (scanning, pairing…)
    Spin,
}

/// A snapshot of one active progress bar, read by the GUI each frame.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[derive(Clone)]
pub struct Gauge {
    pub label: String,
    pub position: u64,
    pub total: Option<u64>,
    pub unit: GaugeUnit,
    pub message: String,
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
struct LiveBar {
    label: String,
    unit: GaugeUnit,
    bar: ProgressBar,
}

static UI_BARS: LazyLock<Mutex<Vec<LiveBar>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Register a bar so the GUI can watch it. Cheap no-op in CLI mode.
pub fn track_gauge(label: &str, bar: &ProgressBar, unit: GaugeUnit) {
    if ui_mode() {
        UI_BARS.lock().unwrap_or_else(|p| p.into_inner()).push(LiveBar {
            label: label.to_string(),
            unit,
            bar: bar.clone(),
        });
    }
}

/// Snapshot the active bars for the GUI; finished bars are dropped.
#[cfg(feature = "gui")]
pub fn ui_gauges() -> Vec<Gauge> {
    if !ui_mode() {
        return Vec::new();
    }
    let mut bars = UI_BARS.lock().unwrap_or_else(|p| p.into_inner());
    let mut active = Vec::new();
    let mut out = Vec::new();
    for lb in bars.drain(..) {
        if lb.bar.is_finished() {
            continue;
        }
        out.push(Gauge {
            label: lb.label.clone(),
            position: lb.bar.position(),
            total: lb.bar.length(),
            unit: lb.unit,
            message: lb.bar.message(),
        });
        active.push(lb);
    }
    *bars = active;
    out
}



pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Free bytes on the volume that will hold `path` (walks up to the nearest existing ancestor).
pub fn free_space(path: &Path) -> Option<u64> {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            return fs2::available_space(&p).ok();
        }
        if !p.pop() {
            return None;
        }
    }
}

/// Make one path component from the device safe for Windows and macOS file systems.
pub fn sanitize_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // Windows forbids trailing dots and spaces.
    if out.is_empty() || out == "." || out == ".." {
        return "_".into();
    }
    if out.ends_with('.') || out.ends_with(' ') {
        out.pop();
        out.push('_');
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }
    out
}

/// Join a device-style relative path ("a/b/c.jpg") onto a local root, safely.
pub fn local_path_for(root: &Path, device_rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in device_rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        p.push(sanitize_component(part));
    }
    p
}

/// True if `rel` stays inside its root (no `..`, no absolute parts).
pub fn is_safe_relative(rel: &Path) -> bool {
    rel.components().all(|c| matches!(c, Component::Normal(_)))
}

pub fn bytes_bar(total: u64, prefix: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{prefix:>10} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}  {wide_msg}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    pb.set_prefix(prefix.to_string());
    pb.enable_steady_tick(Duration::from_millis(250));
    track_gauge(prefix, &pb, GaugeUnit::Bytes);
    pb
}

pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap());
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(120));
    track_gauge("", &pb, GaugeUnit::Spin);
    pb
}

/// Per-stage results, written into report.json.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    pub files_copied: u64,
    pub files_skipped_unchanged: u64,
    pub bytes_copied: u64,
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

impl Stats {
    pub fn merge(&mut self, other: Stats) {
        self.files_copied += other.files_copied;
        self.files_skipped_unchanged += other.files_skipped_unchanged;
        self.bytes_copied += other.bytes_copied;
        self.errors.extend(other.errors);
        self.notes.extend(other.notes);
    }

    pub fn summary(&self) -> String {
        format!(
            "{} copied ({}), {} unchanged, {} errors",
            self.files_copied,
            human_bytes(self.bytes_copied),
            self.files_skipped_unchanged,
            self.errors.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_windows_hostile_names() {
        assert_eq!(sanitize_component("a:b?.txt"), "a_b_.txt");
        assert_eq!(sanitize_component("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_component("trail."), "trail_");
        assert_eq!(sanitize_component(".."), "_");
    }

    #[test]
    fn local_path_drops_traversal() {
        let p = local_path_for(Path::new("/root"), "../../etc/passwd");
        assert_eq!(p, Path::new("/root/etc/passwd"));
    }
}
