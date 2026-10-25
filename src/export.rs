//! Turn a backup folder (hashed file names) into a browsable tree using Manifest.db.
//! Works on unencrypted backups; encrypted ones need decryption tooling first.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::util::{bytes_bar, local_path_for, sanitize_component, Stats};

pub fn run(backup_dir: &Path, out: &Path, domain_filter: &[String]) -> Result<Stats> {
    let manifest = backup_dir.join("Manifest.plist");
    if !manifest.exists() {
        bail!(
            "{} is not a backup folder (no Manifest.plist). Point --backup at the <UDID> folder.",
            backup_dir.display()
        );
    }
    let m: plist::Dictionary = plist::from_file(&manifest).context("reading Manifest.plist")?;
    if m.get("IsEncrypted").and_then(|v| v.as_boolean()) == Some(true) {
        bail!(
            "This backup is encrypted, so its files can't be exported directly. \
             Restore it with Finder/Apple Devices, or decrypt it with a tool such as \
             `iphone_backup_decrypt` first."
        );
    }

    let db = Connection::open_with_flags(backup_dir.join("Manifest.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening Manifest.db")?;
    // flags: 1 = file, 2 = directory, 4 = symlink
    let mut q = db.prepare("SELECT fileID, domain, relativePath FROM Files WHERE flags = 1")?;
    let rows: Vec<(String, String, String)> = q
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    let rows: Vec<_> = rows
        .into_iter()
        .filter(|(_, d, _)| domain_filter.is_empty() || domain_filter.iter().any(|f| d.contains(f.as_str())))
        .collect();

    let mut stats = Stats::default();
    let sizes: Vec<u64> = rows
        .iter()
        .map(|(id, _, _)| blob_path(backup_dir, id).metadata().map(|m| m.len()).unwrap_or(0))
        .collect();
    let bar = bytes_bar(sizes.iter().sum(), "export");

    for ((id, domain, rel), size) in rows.iter().zip(sizes) {
        let src = blob_path(backup_dir, id);
        let dest = local_path_for(&out.join(sanitize_component(domain)), rel);
        if !src.exists() {
            // Normal for incremental snapshots: unchanged files can live only in Manifest.db.
            bar.inc(size);
            continue;
        }
        if dest.metadata().map(|m| m.len() == size).unwrap_or(false) {
            stats.files_skipped_unchanged += 1;
            bar.inc(size);
            continue;
        }
        let r = (|| -> std::io::Result<()> {
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&src, &dest)?;
            Ok(())
        })();
        match r {
            Ok(()) => {
                stats.files_copied += 1;
                stats.bytes_copied += size;
            }
            Err(e) => stats.errors.push(format!("{domain}/{rel}: {e}")),
        }
        bar.inc(size);
    }
    bar.finish_and_clear();
    Ok(stats)
}

/// Blobs live at <backup>/<first two hex chars>/<fileID>.
fn blob_path(backup_dir: &Path, id: &str) -> std::path::PathBuf {
    backup_dir.join(id.get(..2).unwrap_or("")).join(id)
}
