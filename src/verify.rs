//! Prove that each copied file on the PC is byte-identical to the original on the iPhone.
//!
//! Preferred: the iPhone hashes the file itself (AFC GetFileHash), and we hash the local copy
//! with the same algorithm. That's an independent check of the original, not of our transfer.
//! Fallback (if the device won't hash a file): read it from the device again and compare
//! SHA-256 of that fresh read against SHA-256 of the local copy.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Result};
use idevice::afc::{opcode::AfcFopenMode, AfcClient};
use idevice::IdeviceError;
use indicatif::ProgressBar;
use md5::Md5;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::afc_copy::{self, is_fatal, RemoteFile};
use crate::util::{bytes_bar, local_path_for, Stats};

const MAX_ATTEMPTS: u32 = 4;
pub const MANIFEST: &str = "verified.json";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum Algo {
    Md5,
    Sha1,
    Sha256,
}

impl Algo {
    fn from_len(n: usize) -> Option<Self> {
        match n {
            16 => Some(Algo::Md5),
            20 => Some(Algo::Sha1),
            32 => Some(Algo::Sha256),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verified {
    pub size: u64,
    /// Seconds since the Unix epoch (file time on the iPhone).
    pub mtime: i64,
    pub algo: Algo,
    pub hash: String,
    /// "device-hash" (iPhone computed it) or "re-read" (fallback).
    pub method: String,
    pub verified_at: String,
}

/// device path -> verification record. Kept across runs in <out>/verified.json.
pub type Manifest = BTreeMap<String, Verified>;

pub fn load_manifest(out: &Path) -> Manifest {
    std::fs::read(out.join(MANIFEST))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_manifest(out: &Path, m: &Manifest) -> Result<()> {
    let tmp = out.join(format!("{MANIFEST}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(tmp, out.join(MANIFEST))?;
    Ok(())
}

pub(crate) fn hash_local(path: &Path, algo: Algo) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    macro_rules! run {
        ($h:expr) => {{
            let mut h = $h;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            h.finalize().to_vec()
        }};
    }
    Ok(match algo {
        Algo::Md5 => run!(Md5::new()),
        Algo::Sha1 => run!(Sha1::new()),
        Algo::Sha256 => run!(Sha256::new()),
    })
}

/// SHA-256 of a fresh read of the file from the device.
pub(crate) async fn hash_device_reread(afc: &mut AfcClient, path: &str) -> Result<Vec<u8>, IdeviceError> {
    let mut fd = afc.open(path.to_string(), AfcFopenMode::RdOnly).await?;
    let mut h = Sha256::new();
    loop {
        let chunk = fd.read_n(4 * 1024 * 1024).await?;
        if chunk.is_empty() {
            break;
        }
        h.update(&chunk);
    }
    fd.close().await?;
    Ok(h.finalize().to_vec())
}

enum Outcome {
    Match(Verified),
    Mismatch,
    /// Device error that is about this file only (e.g. it was deleted on the phone meanwhile).
    FileError(String),
}

async fn check_one(afc: &mut AfcClient, f: &RemoteFile, local: &Path) -> Result<Outcome, IdeviceError> {
    let meta = match std::fs::metadata(local) {
        Ok(m) => m,
        Err(_) => return Ok(Outcome::Mismatch), // never copied, or vanished
    };
    if meta.len() != f.size {
        return Ok(Outcome::Mismatch);
    }

    // 1) Ask the iPhone to hash its original.
    let (device_hash, algo, method) = match afc.get_file_hash(f.path.clone()).await {
        Ok(h) if Algo::from_len(h.len()).is_some() => {
            let a = Algo::from_len(h.len()).unwrap();
            (h, a, "device-hash")
        }
        Err(e) if is_fatal(&e) => return Err(e),
        // 2) Device can't hash this file: read it again and compare.
        _ => match hash_device_reread(afc, &f.path).await {
            Ok(h) => (h, Algo::Sha256, "re-read"),
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => return Ok(Outcome::FileError(e.to_string())),
        },
    };

    let local_path = local.to_path_buf();
    let local_hash = tokio::task::spawn_blocking(move || hash_local(&local_path, algo))
        .await
        .map_err(|e| IdeviceError::InternalError(e.to_string()))?;
    let local_hash = match local_hash {
        Ok(h) => h,
        Err(e) => return Ok(Outcome::FileError(format!("reading local copy: {e}"))),
    };

    if local_hash == device_hash {
        Ok(Outcome::Match(Verified {
            size: f.size,
            mtime: f.mtime,
            algo,
            hash: hex::encode(device_hash),
            method: method.into(),
            verified_at: chrono::Local::now().to_rfc3339(),
        }))
    } else {
        Ok(Outcome::Mismatch)
    }
}

pub struct VerifyReport {
    pub verified: Vec<String>,
    pub failed: Vec<String>,
}

/// Verify every file in `files` (already copied under `out`). A mismatching file is
/// re-copied once and checked again. Results are merged into <out>/verified.json.
pub async fn run(
    p: &idevice::provider::UsbmuxdProvider,
    files: &[RemoteFile],
    out: &Path,
    stats: &mut Stats,
) -> Result<VerifyReport> {
    let mut manifest = load_manifest(out);
    let mut report = VerifyReport { verified: Vec::new(), failed: Vec::new() };
    let bar = bytes_bar(files.iter().map(|f| f.size).sum(), "verify");

    let mut i = 0usize;
    let mut attempt = 0;
    'outer: while i < files.len() {
        attempt += 1;
        let mut afc: AfcClient = crate::device::connect(p, "file access (AFC)").await?;
        while i < files.len() {
            let f = &files[i];
            let local = local_path_for(out, &f.path);
            bar.set_message(f.path.clone());

            let mut outcome = check_one(&mut afc, f, &local).await;
            if matches!(outcome, Ok(Outcome::Mismatch)) {
                // Re-copy once, then check again.
                bar.println(format!("  mismatch, re-copying {}", f.path));
                let _ = std::fs::remove_file(&local);
                let silent = ProgressBar::hidden();
                let mut s = Stats::default();
                match afc_copy::copy_all(&mut afc, std::slice::from_ref(f), "", out, &silent, &mut s).await {
                    Ok(()) => {
                        stats.files_copied += s.files_copied;
                        stats.bytes_copied += s.bytes_copied;
                        outcome = check_one(&mut afc, f, &local).await;
                    }
                    Err(e) => outcome = Err(e),
                }
            }

            match outcome {
                Ok(Outcome::Match(v)) => {
                    manifest.insert(f.path.clone(), v);
                    report.verified.push(f.path.clone());
                }
                Ok(Outcome::Mismatch) => {
                    manifest.remove(&f.path);
                    report.failed.push(f.path.clone());
                    stats.errors.push(format!("{}: copy does not match the iPhone original", f.path));
                }
                Ok(Outcome::FileError(e)) => {
                    manifest.remove(&f.path);
                    report.failed.push(f.path.clone());
                    stats.errors.push(format!("verify {}: {e}", f.path));
                }
                Err(e) if attempt < MAX_ATTEMPTS => {
                    bar.println(format!("  connection lost while verifying ({e}); resuming…"));
                    let _ = save_manifest(out, &manifest);
                    afc_copy::retry_pause().await;
                    continue 'outer; // reconnect, retry this same file
                }
                Err(e) => {
                    bar.finish_and_clear();
                    save_manifest(out, &manifest)?;
                    bail!("verification failed repeatedly: {e}");
                }
            }
            bar.inc(f.size);
            i += 1;
            if i % 200 == 0 {
                save_manifest(out, &manifest)?; // checkpoint
            }
        }
    }
    bar.finish_and_clear();
    save_manifest(out, &manifest)?;
    Ok(report)
}

/// "Everything in DCIM older than this is verified": the time just before the oldest
/// DCIM file that is NOT verified. None if every DCIM file is verified.
pub fn safe_before(files: &[RemoteFile], failed: &[String]) -> Option<i64> {
    files
        .iter()
        .filter(|f| f.path.starts_with("DCIM/") && failed.contains(&f.path))
        .map(|f| f.mtime)
        .min()
}

pub fn fmt_time(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hashes_match_known_vectors() {
        let dir = std::env::temp_dir().join("ibackup-verify-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(hex::encode(hash_local(&p, Algo::Md5).unwrap()), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex::encode(hash_local(&p, Algo::Sha1).unwrap()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex::encode(hash_local(&p, Algo::Sha256).unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Algo::from_len(20), Some(Algo::Sha1));
    }

    #[test]
    fn safe_before_is_oldest_unverified_dcim_file() {
        let f = |p: &str, t| RemoteFile { path: p.into(), size: 1, mtime: t };
        let files = vec![f("DCIM/a.jpg", 100), f("DCIM/b.jpg", 200), f("PhotoData/x", 50)];
        assert_eq!(safe_before(&files, &[]), None);
        assert_eq!(safe_before(&files, &["DCIM/b.jpg".into(), "PhotoData/x".into()]), Some(200));
    }
}
