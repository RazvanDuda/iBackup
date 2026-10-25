//! Incremental copy of a directory tree from the device over AFC.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use idevice::afc::{opcode::AfcFopenMode, AfcClient};
use idevice::IdeviceError;
use indicatif::ProgressBar;
use tokio::io::AsyncWriteExt;

use crate::util::{local_path_for, Stats};

#[derive(Debug, Clone)]
pub struct RemoteFile {
    /// Path on the device, relative to the AFC root (e.g. "DCIM/100APPLE/IMG_0001.HEIC").
    pub path: String,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub mtime: i64,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

/// Is this an error that means the connection itself is gone (so we should reconnect)?
/// File-level errors (permission denied, file vanished) are not fatal: we log and move on.
pub fn is_fatal(e: &IdeviceError) -> bool {
    matches!(
        e,
        IdeviceError::Socket(_)
            | IdeviceError::Rustls(_)
            | IdeviceError::Timeout
            | IdeviceError::NoEstablishedConnection
            | IdeviceError::SessionInactive
    )
}

/// Recursively list regular files under `root`. Skips directories matching `exclude`.
pub async fn scan(
    afc: &mut AfcClient,
    root: &str,
    exclude: &[String],
    stats: &mut Stats,
) -> Result<Vec<RemoteFile>, IdeviceError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_string()];
    while let Some(dir) = stack.pop() {
        let names = match afc.list_dir(dir.clone()).await {
            Ok(n) => n,
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => {
                stats.errors.push(format!("list {dir}: {e}"));
                continue;
            }
        };
        for name in names {
            if name == "." || name == ".." {
                continue;
            }
            let path = join(&dir, &name);
            if exclude.iter().any(|x| path.starts_with(x.trim_start_matches('/'))) {
                continue;
            }
            let info = match afc.get_file_info(path.clone()).await {
                Ok(i) => i,
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => {
                    stats.errors.push(format!("stat {path}: {e}"));
                    continue;
                }
            };
            match info.st_ifmt.as_str() {
                "S_IFDIR" => stack.push(path),
                "S_IFREG" => out.push(RemoteFile {
                    path,
                    size: info.size as u64,
                    mtime: info.modified.and_utc().timestamp(),
                }),
                _ => {} // symlinks, sockets, etc.
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Unchanged if a local file exists with the same size and (to the second) the same mtime.
fn unchanged(local: &Path, f: &RemoteFile) -> bool {
    let Ok(m) = std::fs::metadata(local) else { return false };
    if m.len() != f.size {
        return false;
    }
    match m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => (d.as_secs() as i64 - f.mtime).abs() <= 2,
        None => false,
    }
}

async fn copy_one(afc: &mut AfcClient, f: &RemoteFile, dest: &Path, bar: &ProgressBar) -> Result<(), CopyErr> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(CopyErr::Local)?;
    }
    let tmp: PathBuf = {
        let mut s = dest.as_os_str().to_owned();
        s.push(".ibackup-part");
        s.into()
    };
    let mut fd = afc.open(f.path.clone(), AfcFopenMode::RdOnly).await.map_err(CopyErr::Device)?;
    let mut out = tokio::fs::File::create(&tmp).await.map_err(CopyErr::Local)?;

    // Stream in chunks so huge videos never sit in memory.
    const CHUNK: usize = 4 * 1024 * 1024;
    loop {
        let buf = fd.read_n(CHUNK).await.map_err(CopyErr::Device)?;
        if buf.is_empty() {
            break;
        }
        out.write_all(&buf).await.map_err(CopyErr::Local)?;
        bar.inc(buf.len() as u64);
        // Short reads can happen mid-file; only an empty read means EOF.
    }
    out.flush().await.map_err(CopyErr::Local)?;
    drop(out);
    fd.close().await.map_err(CopyErr::Device)?;

    tokio::fs::rename(&tmp, dest).await.map_err(CopyErr::Local)?;
    let t = filetime::FileTime::from_unix_time(f.mtime, 0);
    let _ = filetime::set_file_mtime(dest, t);
    Ok(())
}

enum CopyErr {
    Device(IdeviceError),
    Local(std::io::Error),
}

/// Copy `files` into `local_root`. Returns Err only if the device connection dropped,
/// so the caller can reconnect and call again (finished files are skipped next time).
pub async fn copy_all(
    afc: &mut AfcClient,
    files: &[RemoteFile],
    strip_prefix: &str,
    local_root: &Path,
    bar: &ProgressBar,
    stats: &mut Stats,
) -> Result<(), IdeviceError> {
    for f in files {
        let rel = f.path.strip_prefix(strip_prefix).unwrap_or(&f.path).trim_start_matches('/');
        let dest = local_path_for(local_root, rel);
        if unchanged(&dest, f) {
            stats.files_skipped_unchanged += 1;
            bar.inc(f.size);
            continue;
        }
        bar.set_message(rel.to_string());
        let before = bar.position();
        match copy_one(afc, f, &dest, bar).await {
            Ok(()) => {
                stats.files_copied += 1;
                stats.bytes_copied += f.size;
                bar.set_position(before + f.size);
            }
            Err(CopyErr::Device(e)) if is_fatal(&e) => {
                bar.set_position(before);
                return Err(e);
            }
            Err(CopyErr::Device(e)) => {
                stats.errors.push(format!("{}: {e}", f.path));
                bar.set_position(before + f.size);
            }
            Err(CopyErr::Local(e)) => {
                stats.errors.push(format!("{} -> {}: {e}", f.path, dest.display()));
                bar.set_position(before + f.size);
            }
        }
    }
    Ok(())
}

/// Total bytes of files that still need copying.
pub fn pending_bytes(files: &[RemoteFile]) -> u64 {
    files.iter().map(|f| f.size).sum()
}

pub async fn retry_pause() {
    tokio::time::sleep(Duration::from_secs(3)).await;
}

pub fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))
}
