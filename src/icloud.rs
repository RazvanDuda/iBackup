//! iCloud data is not available through any USB backup protocol; Apple's backup leaves
//! out anything already stored in iCloud. This module covers it from the PC side:
//!
//! 1. Copies the folders that iCloud already syncs to this computer
//!    (macOS: built in; Windows: the "iCloud for Windows" app).
//!    Reading a cloud-only placeholder makes the OS download it, so this pulls a
//!    full local copy of everything in iCloud Drive.
//! 2. Optionally runs `icloudpd` to download the full iCloud Photos library.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};

use crate::util::{bytes_bar, human_bytes, is_safe_relative, Stats};

#[derive(Debug, Clone)]
pub struct Source {
    pub label: String,
    pub path: PathBuf,
}

/// iCloud folders that this computer keeps in sync, if present.
pub fn detect_sources() -> Vec<Source> {
    let mut v = Vec::new();
    let Some(home) = dirs::home_dir() else { return v };
    let mut add = |label: &str, p: PathBuf| {
        if p.is_dir() {
            v.push(Source { label: label.into(), path: p });
        }
    };
    if cfg!(target_os = "macos") {
        // Holds iCloud Drive (com~apple~CloudDocs) plus every app's iCloud container.
        add("iCloud Drive", home.join("Library/Mobile Documents"));
    }
    if cfg!(windows) {
        add("iCloud Drive", home.join("iCloudDrive"));
        add("iCloud Photos", home.join("Pictures").join("iCloud Photos").join("Photos"));
        add("iCloud Photos (shared)", home.join("Pictures").join("iCloud Photos").join("Shared"));
        // Older iCloud for Windows layouts.
        add("iCloud Photos (downloads)", home.join("Pictures").join("iCloud Photos").join("Downloads"));
    }
    v
}

fn skip_name(name: &str) -> bool {
    matches!(name, ".DS_Store" | "desktop.ini" | "Thumbs.db") || name.ends_with(".ibackup-part")
}

/// Old-style macOS placeholder for a file that hasn't been downloaded: ".Name.ext.icloud".
fn is_placeholder(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".icloud")
}

#[cfg(target_os = "macos")]
fn request_download(path: &Path) {
    // Asks the iCloud daemon to materialize everything under `path`.
    let _ = std::process::Command::new("brctl").arg("download").arg(path).status();
}
#[cfg(not(target_os = "macos"))]
fn request_download(_path: &Path) {}

struct Entry {
    rel: PathBuf,
    size: u64,
    mtime: Option<std::time::SystemTime>,
}

fn walk(root: &Path, stats: &mut Stats, placeholders: &mut u64) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                stats.errors.push(format!("read {}: {e}", dir.display()));
                continue;
            }
        };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if skip_name(&name) {
                continue;
            }
            if is_placeholder(&name) {
                *placeholders += 1;
                continue;
            }
            let Ok(ft) = ent.file_type() else { continue };
            let p = ent.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                let m = ent.metadata().ok();
                let rel = p.strip_prefix(root).unwrap_or(&p).to_path_buf();
                if !is_safe_relative(&rel) {
                    continue;
                }
                out.push(Entry {
                    rel,
                    size: m.as_ref().map_or(0, |m| m.len()),
                    mtime: m.and_then(|m| m.modified().ok()),
                });
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn unchanged(dest: &Path, e: &Entry) -> bool {
    let Ok(m) = std::fs::metadata(dest) else { return false };
    if m.len() != e.size {
        return false;
    }
    let secs = |t: std::time::SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).ok();
    match (m.modified().ok().and_then(secs), e.mtime.and_then(secs)) {
        (Some(a), Some(b)) => (a - b).abs() <= 2,
        _ => false,
    }
}

fn copy_tree(src: &Source, dest_root: &Path, download: bool) -> Stats {
    let mut stats = Stats::default();
    let mut placeholders = 0;
    if download {
        request_download(&src.path);
    }
    let files = walk(&src.path, &mut stats, &mut placeholders);
    if placeholders > 0 {
        stats.notes.push(format!(
            "{}: {placeholders} files are cloud-only placeholders and were not copied{}",
            src.label,
            if cfg!(target_os = "macos") { "; rerun with --download" } else { "" }
        ));
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    crate::util::say(&format!("  {}: {} files, {}  ({})", src.label, files.len(), human_bytes(total), src.path.display()));
    let bar = bytes_bar(total, "icloud");
    for f in &files {
        let from = src.path.join(&f.rel);
        let to = dest_root.join(&f.rel);
        if unchanged(&to, f) {
            stats.files_skipped_unchanged += 1;
            bar.inc(f.size);
            continue;
        }
        bar.set_message(f.rel.display().to_string());
        let res = (|| -> std::io::Result<()> {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut tmp = to.as_os_str().to_owned();
            tmp.push(".ibackup-part");
            // Reading a cloud-only file makes macOS / Windows download it first.
            std::fs::copy(&from, &tmp)?;
            std::fs::rename(&tmp, &to)?;
            if let Some(t) = f.mtime {
                let _ = filetime::set_file_mtime(&to, filetime::FileTime::from_system_time(t));
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                stats.files_copied += 1;
                stats.bytes_copied += f.size;
            }
            Err(e) => stats.errors.push(format!("{}: {e}", from.display())),
        }
        bar.inc(f.size);
    }
    bar.finish_and_clear();
    stats
}

pub struct Options {
    pub extra_sources: Vec<PathBuf>,
    pub no_auto_detect: bool,
    pub download: bool,
    pub icloudpd_user: Option<String>,
    pub rclone_remote: Option<String>,
}

pub async fn run(out: &Path, opts: &Options) -> Result<Stats> {
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let mut sources = if opts.no_auto_detect { Vec::new() } else { detect_sources() };
    for p in &opts.extra_sources {
        if !p.is_dir() {
            bail!("--source {} is not a folder", p.display());
        }
        let label = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or("source".into());
        sources.push(Source { label, path: p.clone() });
    }

    let mut stats = Stats::default();
    if sources.is_empty() && opts.icloudpd_user.is_none() && opts.rclone_remote.is_none() {
        let hint = if cfg!(windows) {
            "Install \"iCloud for Windows\" from the Microsoft Store and turn on iCloud Drive and Photos, \
             or use --icloudpd <apple-id>."
        } else if cfg!(target_os = "macos") {
            "Sign in to iCloud in System Settings and turn on iCloud Drive, or use --icloudpd <apple-id>."
        } else {
            "Apple has no iCloud app for Linux: use --rclone <remote> (rclone's iclouddrive backend) \
             for iCloud Drive, --icloudpd <apple-id> for iCloud Photos, or --source <folder>."
        };
        stats.notes.push(format!("no iCloud folders found on this computer. {hint}"));
        crate::util::say(&format!("  no iCloud folders found. {hint}"));
        return Ok(stats);
    }

    for src in sources {
        let dest = out.join(crate::util::sanitize_component(&src.label));
        let download = opts.download;
        let s = tokio::task::spawn_blocking(move || copy_tree(&src, &dest, download)).await?;
        stats.merge(s);
    }

    if let Some(remote) = &opts.rclone_remote {
        stats.merge(run_rclone(remote, &out.join("iCloud Drive (rclone)")).await);
    }
    if let Some(user) = &opts.icloudpd_user {
        stats.merge(run_icloudpd(user, &out.join("iCloud Photos (icloudpd)")).await);
    }
    Ok(stats)
}

/// iCloud Drive download via rclone's `iclouddrive` backend (rclone 1.69+). Set the remote up
/// once with `rclone config`; rclone holds the Apple sign-in token, not us.
async fn run_rclone(remote: &str, dest: &Path) -> Stats {
    let mut stats = Stats::default();
    let _ = std::fs::create_dir_all(dest);
    let src = if remote.contains(':') { remote.to_string() } else { format!("{remote}:") };
    crate::util::say(&format!("  running rclone copy {src} …"));
    let status = tokio::process::Command::new("rclone")
        .arg("copy")
        .arg(&src)
        .arg(dest)
        .arg("--progress")
        .status()
        .await;
    match status {
        Ok(s) if s.success() => stats.notes.push(format!("rclone copied {src}")),
        Ok(s) => stats.errors.push(format!(
            "rclone exited with {s}. If sign-in expired (tokens last 30 days), run `rclone reconnect {src}`"
        )),
        Err(e) => stats.errors.push(format!(
            "could not run rclone ({e}). Install it (Fedora: `sudo dnf install rclone`) and create an \
             iclouddrive remote with `rclone config`."
        )),
    }
    stats
}

/// Full-resolution iCloud Photos download via icloudpd (https://github.com/icloud-photos-downloader/icloud_photos_downloader).
/// It handles Apple sign-in and two-factor prompts itself; we never see the password.
async fn run_icloudpd(user: &str, dest: &Path) -> Stats {
    let mut stats = Stats::default();
    let _ = std::fs::create_dir_all(dest);
    crate::util::say("  running icloudpd for iCloud Photos (it will prompt for sign-in and 2FA)…");
    let status = tokio::process::Command::new("icloudpd")
        .arg("--directory")
        .arg(dest)
        .arg("--username")
        .arg(user)
        .arg("--folder-structure")
        .arg("{:%Y/%m}")
        .status()
        .await;
    match status {
        Ok(s) if s.success() => stats.notes.push("icloudpd finished".into()),
        Ok(s) => stats.errors.push(format!("icloudpd exited with {s}")),
        Err(e) => stats.errors.push(format!(
            "could not run icloudpd ({e}). Install it with `pip install icloudpd` or from its GitHub releases."
        )),
    }
    stats
}
