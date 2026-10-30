//! Delete camera-roll files from the iPhone that are proven to be safely on this computer.
//!
//! Safeguards, in order:
//! 1. Only files recorded in verified.json whose size and date on the phone still match.
//! 2. Whole groups only: IMG_0001.HEIC + IMG_0001.MOV (Live Photo) + IMG_E0001.JPG (edit) +
//!    IMG_0001.AAE + its PhotoData/Mutations folder are deleted together or not at all.
//! 3. Right before deleting, every file in the group is hashed again on BOTH sides
//!    (iPhone original and local copy) and must still equal the recorded hash.
//! 4. A plan is shown and the user must type a confirmation (unless --yes).
//! 5. Every deletion is logged to deleted.json. Any connection error stops; nothing is retried blindly.
//!
//! `--delete-all` relaxes 1 and 3 (the iPhone itself may be unable to hash its files) but never
//! 2, 4 or 5: a group is only deleted when every one of its files is present on this computer with
//! the same size and date, and the local copy still matches its recorded hash when we have one.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use idevice::afc::AfcClient;
use idevice::IdeviceError;
use serde::Serialize;

use crate::afc_copy::{self, is_fatal, RemoteFile};
use crate::util::{bytes_bar, human_bytes, local_path_for, Stats};
use crate::verify::{self, Algo, Manifest};

pub const DELETED_LOG: &str = "deleted.json";

pub struct Options {
    /// Only delete files dated before this (Unix seconds).
    pub before: Option<i64>,
    pub yes: bool,
    pub rebuild_db: bool,
    /// Delete every camera-roll item that has a copy on this computer, verified or not.
    pub all: bool,
}

/// "DCIM/100APPLE/IMG_E0001.JPG" -> ("100APPLE", "IMG_0001")
fn group_key(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("DCIM/")?;
    let (dir, name) = rest.rsplit_once('/')?;
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let norm = match stem.strip_prefix("IMG_E").or_else(|| stem.strip_prefix("IMG_O")) {
        Some(n) if n.chars().all(|c| c.is_ascii_digit()) => format!("IMG_{n}"),
        _ => stem.to_string(),
    };
    Some((dir.to_string(), norm))
}

/// Edits live at PhotoData/Mutations/DCIM/<dir>/<stem>/...
fn mutation_key(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("PhotoData/Mutations/DCIM/")?;
    let mut parts = rest.splitn(3, '/');
    let dir = parts.next()?;
    let stem = parts.next()?;
    parts.next()?; // must be a file inside the stem folder
    group_key(&format!("DCIM/{dir}/{stem}.x"))
}

fn still_matches(f: &RemoteFile, m: &Manifest) -> bool {
    m.get(&f.path).is_some_and(|v| v.size == f.size && (v.mtime - f.mtime).abs() <= 2)
}

struct Group {
    files: Vec<RemoteFile>,
    newest: i64,
    bytes: u64,
}

/// Files grouped by the photo they belong to (Live Photo halves, edits, mutations).
fn group_files(files: &[RemoteFile]) -> BTreeMap<(String, String), Vec<RemoteFile>> {
    let mut groups: BTreeMap<(String, String), Vec<RemoteFile>> = BTreeMap::new();
    for f in files {
        if let Some(k) = group_key(&f.path).or_else(|| mutation_key(&f.path)) {
            groups.entry(k).or_default().push(f.clone());
        }
    }
    groups
}

/// Groups where every member is verified and unchanged, and (optionally) older than `before`.
fn plan(files: &[RemoteFile], m: &Manifest, before: Option<i64>) -> (Vec<Group>, usize) {
    let groups = group_files(files);
    let mut ok = Vec::new();
    let mut skipped = 0;
    for (_, members) in groups {
        // A group with only Mutations and no DCIM original is not ours to touch.
        if !members.iter().any(|f| f.path.starts_with("DCIM/")) {
            continue;
        }
        let newest = members.iter().map(|f| f.mtime).max().unwrap_or(0);
        if before.is_some_and(|b| newest >= b) {
            continue;
        }
        if members.iter().all(|f| still_matches(f, m)) {
            let bytes = members.iter().map(|f| f.size).sum();
            ok.push(Group { files: members, newest, bytes });
        } else {
            skipped += 1;
        }
    }
    (ok, skipped)
}

/// `--delete-all` plan: every group that is present on this computer, verified or not.
///
/// Returns the groups to delete plus how many groups / files were kept because the copy here is
/// missing, has a different size or date, or no longer matches the hash we recorded for it.
fn plan_all(files: &[RemoteFile], out: &Path, before: Option<i64>) -> (Vec<Group>, usize, usize) {
    let mut ok = Vec::new();
    let mut kept_groups = 0;
    let mut kept_files = 0;
    for (_, members) in group_files(files) {
        // Orphaned edits (mutations with no original) are deleted too: --delete-all empties the roll.
        let newest = members.iter().map(|f| f.mtime).max().unwrap_or(0);
        if before.is_some_and(|b| newest >= b) {
            continue;
        }
        let not_here: Vec<&RemoteFile> = members
            .iter()
            .filter(|f| !afc_copy::unchanged(&local_path_for(out, &f.path), f))
            .collect();
        if !not_here.is_empty() {
            kept_groups += 1;
            kept_files += members.len();
            continue;
        }
        let bytes = members.iter().map(|f| f.size).sum();
        ok.push(Group { files: members, newest, bytes });
    }
    (ok, kept_groups, kept_files)
}

/// Proof for `--delete-all`, where the iPhone may be unable to hash its own files: the local copy
/// must still have the size and date the phone reports, and (when verified.json has a record for
/// it) still hash to exactly what we recorded. Returns the hash to log.
fn local_proof(f: &RemoteFile, m: &Manifest, out: &Path) -> Option<String> {
    let local = local_path_for(out, &f.path);
    if !afc_copy::unchanged(&local, f) {
        return None;
    }
    let (algo, recorded) = match m.get(&f.path) {
        Some(v) => (v.algo, Some(v.hash.clone())),
        None => (Algo::Sha256, None),
    };
    let got = hex::encode(verify::hash_local(&local, algo).ok()?);
    match recorded {
        Some(want) if want != got => None,
        _ => Some(got),
    }
}

/// Re-hash one file on both sides and compare with the recorded hash.
async fn recheck(afc: &mut AfcClient, f: &RemoteFile, m: &Manifest, out: &Path) -> Result<bool, IdeviceError> {
    let Some(v) = m.get(&f.path) else { return Ok(false) };
    let device = if v.method == "device-hash" {
        match afc.get_file_hash(f.path.clone()).await {
            Ok(h) => h,
            Err(e) if is_fatal(&e) => return Err(e),
            Err(_) => return Ok(false),
        }
    } else {
        match verify::hash_device_reread(afc, &f.path).await {
            Ok(h) => h,
            Err(e) if is_fatal(&e) => return Err(e),
            Err(_) => return Ok(false),
        }
    };
    if hex::encode(&device) != v.hash {
        return Ok(false);
    }
    let local = local_path_for(out, &f.path);
    let algo: Algo = v.algo;
    let local_hash = tokio::task::spawn_blocking(move || verify::hash_local(&local, algo))
        .await
        .ok()
        .and_then(|r| r.ok());
    Ok(local_hash.is_some_and(|h| hex::encode(h) == v.hash))
}

fn confirm(
    n_files: usize,
    n_groups: usize,
    bytes: u64,
    oldest: i64,
    newest: i64,
    rebuild_db: bool,
    all: bool,
) -> Result<bool> {
    eprintln!();
    eprintln!("About to DELETE from the iPhone: {n_files} files ({n_groups} photos/videos, {})", human_bytes(bytes));
    eprintln!("  dated {} … {}", verify::fmt_time(oldest), verify::fmt_time(newest));
    if all {
        eprintln!("  --delete-all: every camera-roll item that has a copy on this computer goes, verified or not.");
        eprintln!("  Right before deletion each local copy is hashed again and must still be the file recorded here;");
        eprintln!("  items without such a copy stay on the iPhone.");
    } else {
        eprintln!("  Each one is re-checked against its verified copy on this computer right before deletion.");
    }
    eprintln!();
    eprintln!("  WARNING: this deletes the files directly, bypassing the Photos app database.");
    eprintln!("   • If iCloud Photos is ON, turn it OFF first (Settings > Photos). Deleting with it on");
    eprintln!("     can remove these photos from iCloud and your other devices, or leave iCloud out of sync.");
    if rebuild_db {
        eprintln!("   • You asked to rebuild the Photos database: albums, favorites, People and edits");
        eprintln!("     metadata are lost for the photos that stay. Restart the iPhone afterwards.");
    } else {
        eprintln!("   • Photos will show blank thumbnails for deleted items until its database is rebuilt");
        eprintln!("     (rerun with --rebuild-photos-db, then restart the iPhone).");
    }
    eprintln!("  There is no undo on the phone. Your copies stay on this computer.");
    eprintln!();
    let expected = if all { format!("delete all {n_files}") } else { format!("delete {n_files}") };
    eprint!("Type \"{expected}\" to continue: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim() == expected)
}

#[derive(Serialize, serde::Deserialize)]
struct DeletedEntry {
    path: String,
    size: u64,
    hash: String,
    deleted_at: String,
}

fn append_log(out: &Path, entries: &[DeletedEntry]) -> Result<()> {
    let p = out.join(DELETED_LOG);
    let mut all: Vec<DeletedEntry> = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    all.extend(entries.iter().map(|e| DeletedEntry {
        path: e.path.clone(),
        size: e.size,
        hash: e.hash.clone(),
        deleted_at: e.deleted_at.clone(),
    }));
    std::fs::write(&p, serde_json::to_vec_pretty(&all)?)?;
    Ok(())
}

pub async fn run(
    p: &idevice::provider::UsbmuxdProvider,
    files: &[RemoteFile],
    out: &Path,
    opts: &Options,
    stats: &mut Stats,
) -> Result<()> {
    let manifest = verify::load_manifest(out);
    let groups = if opts.all {
        let (groups, kept_groups, kept_files) = plan_all(files, out, opts.before);
        if kept_groups > 0 {
            stats.notes.push(format!(
                "{kept_groups} photos/videos ({kept_files} files) kept on the iPhone: no matching copy on this computer"
            ));
        }
        groups
    } else {
        let (groups, skipped) = plan(files, &manifest, opts.before);
        if skipped > 0 {
            stats.notes.push(format!("{skipped} photos/videos kept on the iPhone: not every part of them is verified"));
        }
        groups
    };
    if groups.is_empty() {
        stats.notes.push("nothing to delete from the iPhone".into());
        return Ok(());
    }
    let n_files: usize = groups.iter().map(|g| g.files.len()).sum();
    let bytes: u64 = groups.iter().map(|g| g.bytes).sum();
    let oldest = groups.iter().map(|g| g.newest).min().unwrap_or(0);
    let newest = groups.iter().map(|g| g.newest).max().unwrap_or(0);

    if !opts.yes && !confirm(n_files, groups.len(), bytes, oldest, newest, opts.rebuild_db, opts.all)? {
        bail!("cancelled; nothing was deleted");
    }

    let mut afc: AfcClient = crate::device::connect(p, "file access (AFC)").await?;
    let bar = bytes_bar(bytes, "delete");
    let mut deleted_files = 0u64;
    let mut deleted_bytes = 0u64;
    let mut log = Vec::new();
    let mut failure: Option<IdeviceError> = None;

    'groups: for g in &groups {
        // Check every member before touching any of them.
        let mut proofs: BTreeMap<String, String> = BTreeMap::new();
        for f in &g.files {
            bar.set_message(format!("checking {}", f.path));
            if opts.all {
                // The iPhone may be the broken half here (that is why --delete-all exists), so the
                // proof comes from the copy on this computer instead of a hash from the phone.
                match local_proof(f, &manifest, out) {
                    Some(h) => {
                        proofs.insert(f.path.clone(), h);
                    }
                    None => {
                        stats
                            .errors
                            .push(format!("{}: no matching copy on this computer, kept on iPhone (with its group)", f.path));
                        bar.inc(g.bytes);
                        continue 'groups;
                    }
                }
                continue;
            }
            match recheck(&mut afc, f, &manifest, out).await {
                Ok(true) => {}
                Ok(false) => {
                    stats.errors.push(format!("{}: re-check failed, kept on iPhone (with its group)", f.path));
                    bar.inc(g.bytes);
                    continue 'groups;
                }
                Err(e) => {
                    failure = Some(e);
                    break 'groups;
                }
            }
        }
        // Delete Live Photo video / edits before the main image so a partial failure
        // never leaves a group's original deleted while its companions remain.
        let mut ordered: Vec<&RemoteFile> = g.files.iter().collect();
        ordered.sort_by_key(|f| (f.path.starts_with("DCIM/") && !f.path.to_ascii_uppercase().ends_with(".MOV"), f.path.clone()));
        for f in ordered {
            bar.set_message(format!("deleting {}", f.path));
            let logged_hash = proofs
                .get(&f.path)
                .cloned()
                .or_else(|| manifest.get(&f.path).map(|v| v.hash.clone()))
                .unwrap_or_default();
            match afc.remove(f.path.clone()).await {
                Ok(()) => {
                    deleted_files += 1;
                    deleted_bytes += f.size;
                    log.push(DeletedEntry {
                        path: f.path.clone(),
                        size: f.size,
                        hash: logged_hash,
                        deleted_at: chrono::Local::now().to_rfc3339(),
                    });
                }
                Err(e) if is_fatal(&e) => {
                    failure = Some(e);
                    break 'groups;
                }
                Err(e) => stats.errors.push(format!("delete {}: {e}", f.path)),
            }
        }
        bar.inc(g.bytes);
        if log.len() >= 100 {
            append_log(out, &log)?;
            log.clear();
        }
    }
    bar.finish_and_clear();
    append_log(out, &log)?;

    stats.notes.push(format!(
        "deleted {deleted_files} {}files ({}) from the iPhone; log in {}",
        if opts.all { "" } else { "verified " },
        human_bytes(deleted_bytes),
        out.join(DELETED_LOG).display()
    ));
    if let Some(e) = failure {
        bail!(
            "the device stopped answering during deletion ({}). Everything deleted so far is logged; \
             rerun to continue.",
            afc_copy::explain(&e)
        );
    }

    if opts.rebuild_db && deleted_files > 0 {
        rebuild_photos_db(&mut afc, out, stats).await?;
    } else if deleted_files > 0 {
        stats.notes.push(
            "the Photos app may show blank thumbnails for deleted items until its database is rebuilt \
             (--rebuild-photos-db, then restart the iPhone)"
                .into(),
        );
    }
    Ok(())
}

/// Copy the Photos database to the computer, then remove it from the phone so iOS
/// rebuilds it from the files that remain on the next restart.
async fn rebuild_photos_db(afc: &mut AfcClient, out: &Path, stats: &mut Stats) -> Result<()> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup_dir = out.join(format!("photos-db-backup-{stamp}"));
    std::fs::create_dir_all(&backup_dir)?;
    let names = afc.list_dir("PhotoData".to_string()).await.context("listing PhotoData")?;
    let targets: BTreeSet<String> = names
        .into_iter()
        .filter(|n| n.starts_with("Photos.sqlite"))
        .map(|n| format!("PhotoData/{n}"))
        .collect();
    if targets.is_empty() {
        stats.notes.push("Photos database not found under PhotoData; nothing rebuilt".into());
        return Ok(());
    }
    // Back up first; abort the rebuild if any backup fails.
    for t in &targets {
        let mut fd = afc.open(t.clone(), idevice::afc::opcode::AfcFopenMode::RdOnly).await?;
        let read = fd.read_entire().await;
        // Close before checking the result: a leaked handle exhausts the phone's AFC service.
        let closed = fd.close().await;
        let data = read?;
        closed?;
        std::fs::write(backup_dir.join(t.trim_start_matches("PhotoData/")), data)
            .with_context(|| format!("backing up {t}"))?;
    }
    for t in &targets {
        afc.remove(t.clone()).await.with_context(|| format!("removing {t}"))?;
    }
    stats.notes.push(format!(
        "Photos database removed from the iPhone (copy saved in {}). RESTART THE IPHONE now; \
         Photos rebuilds its library from the remaining files, which can take a while.",
        backup_dir.display()
    ));
    Ok(())
}

/// Parse YYYY-MM-DD (local midnight) for --delete-before.
pub fn parse_date(s: &str) -> Result<i64> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").context("use the format YYYY-MM-DD")?;
    let dt = d.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(chrono::Local);
    Ok(dt.earliest().context("invalid local date")?.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::Verified;

    fn rf(p: &str, t: i64) -> RemoteFile {
        RemoteFile { path: p.into(), size: 10, mtime: t }
    }
    fn ver_hash(m: &mut Manifest, f: &RemoteFile, hash: String) {
        ver(m, f);
        m.get_mut(&f.path).unwrap().hash = hash;
    }
    fn ver(m: &mut Manifest, f: &RemoteFile) {
        m.insert(
            f.path.clone(),
            Verified {
                size: f.size,
                mtime: f.mtime,
                algo: Algo::Sha1,
                hash: "00".into(),
                method: "device-hash".into(),
                verified_at: String::new(),
            },
        );
    }

    #[test]
    fn groups_live_photos_edits_and_mutations() {
        assert_eq!(group_key("DCIM/100APPLE/IMG_0001.HEIC"), Some(("100APPLE".into(), "IMG_0001".into())));
        assert_eq!(group_key("DCIM/100APPLE/IMG_E0001.JPG"), Some(("100APPLE".into(), "IMG_0001".into())));
        assert_eq!(
            mutation_key("PhotoData/Mutations/DCIM/100APPLE/IMG_0001/Adjustments/FullSizeRender.jpg"),
            Some(("100APPLE".into(), "IMG_0001".into()))
        );
    }

    #[test]
    fn partially_verified_group_is_kept() {
        let heic = rf("DCIM/100APPLE/IMG_0001.HEIC", 100);
        let mov = rf("DCIM/100APPLE/IMG_0001.MOV", 100);
        let solo = rf("DCIM/100APPLE/IMG_0002.HEIC", 100);
        let mut m = Manifest::new();
        ver(&mut m, &heic); // MOV not verified
        ver(&mut m, &solo);
        let (ok, skipped) = plan(&[heic, mov, solo], &m, None);
        assert_eq!(skipped, 1);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].files[0].path, "DCIM/100APPLE/IMG_0002.HEIC");
    }

    /// --delete-all still refuses a group unless every one of its files is here, with the size and
    /// date the phone reports: a missing Live Photo half keeps the whole photo on the iPhone.
    #[test]
    fn delete_all_needs_every_file_present_here() {
        let dir = std::env::temp_dir().join("ibackup-plan-all-test");
        let _ = std::fs::remove_dir_all(&dir);
        let heic = rf("DCIM/100APPLE/IMG_0001.HEIC", 100);
        let mov = rf("DCIM/100APPLE/IMG_0001.MOV", 100);
        let solo = rf("DCIM/100APPLE/IMG_0002.HEIC", 100);
        let orphan = rf("PhotoData/Mutations/DCIM/100APPLE/IMG_0009/Adjustments/FullSizeRender.jpg", 100);

        for f in [&heic, &solo, &orphan] {
            let local = local_path_for(&dir, &f.path);
            std::fs::create_dir_all(local.parent().unwrap()).unwrap();
            std::fs::write(&local, vec![0u8; f.size as usize]).unwrap();
            filetime::set_file_mtime(&local, filetime::FileTime::from_unix_time(f.mtime, 0)).unwrap();
        }
        // IMG_0001.MOV is missing locally, so the pair stays; the solo photo and the orphaned
        // edit (no original left on the phone) go.
        let (ok, kept_groups, kept_files) = plan_all(&[heic.clone(), mov.clone(), solo.clone(), orphan.clone()], &dir, None);
        let mut paths: Vec<&str> = ok.iter().flat_map(|g| g.files.iter().map(|f| f.path.as_str())).collect();
        paths.sort();
        assert_eq!(paths, vec![solo.path.as_str(), orphan.path.as_str()]);
        assert_eq!((kept_groups, kept_files), (1, 2));
        assert!(plan_all(&[solo.clone()], &dir, Some(100)).0.is_empty());

        // Same size, different bytes than the recorded hash: kept.
        let mut m = Manifest::new();
        ver(&mut m, &solo);
        assert!(local_proof(&solo, &m, &dir).is_none());
        // A record whose hash matches the local file: proof, and the hash is what gets logged.
        let real = hex::encode(verify::hash_local(&local_path_for(&dir, &solo.path), Algo::Sha1).unwrap());
        ver_hash(&mut m, &solo, real.clone());
        assert_eq!(local_proof(&solo, &m, &dir), Some(real));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_on_device_is_kept_and_before_filter_applies() {
        let a = rf("DCIM/100APPLE/IMG_0003.HEIC", 100);
        let mut m = Manifest::new();
        ver(&mut m, &a);
        let changed = RemoteFile { size: 11, ..a.clone() };
        assert_eq!(plan(&[changed], &m, None).0.len(), 0);
        assert_eq!(plan(&[a.clone()], &m, Some(100)).0.len(), 0);
        assert_eq!(plan(&[a], &m, Some(101)).0.len(), 1);
    }
}
