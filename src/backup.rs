//! Full device backup using Apple's MobileBackup2 protocol (what Finder and iTunes use).
//! Output is a standard backup folder that Finder / iTunes / Apple Devices can restore.

use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use idevice::mobilebackup2::{
    BackupDelegate, BackupProgress, DirEntryInfo, FsBackupDelegate, MobileBackup2Client,
};
use idevice::usbmuxd::UsbmuxdDevice;
use idevice::IdeviceError;
use indicatif::{ProgressBar, ProgressStyle};
use plist::{Dictionary, Value};
use rusqlite::{Connection, OpenFlags};

use crate::device;
use crate::util;

type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Apple's MBErrorDomain/104 "Multiple errors uploading files". The phone reports one
/// code for a whole batch of per-file failures and never says which files, and the
/// causes mix both sides: files the phone could not read (open database, corrupt
/// block) and files this computer could not save (disk full, permissions, antivirus).
/// It is not fatal: everything that did transfer stays valid, and the folder audit
/// below turns it into an actual list of missing files.
const DEVICE_ERR_MULTIPLE_UPLOAD_ERRORS: u64 = 104;

/// One pass plus one retry. The retry is incremental even for `--full`: it only asks
/// the phone for files still missing, which is what clears the usual cause of error
/// 104 (a database that was open during the first pass).
const MAX_ATTEMPTS: u32 = 2;

/// How many of the missing/failed files to name individually before summarising.
const REPORT_LIMIT: usize = 20;

/// Verbose per-operation tracing of the device link, for diagnosing a stalled or
/// failing backup: `IBACKUP_TRACE=1 ibackup backup --out …`.
macro_rules! trace {
    ($($t:tt)*) => {
        if std::env::var_os("IBACKUP_TRACE").is_some() {
            eprintln!("[trace] {}", format_args!($($t)*));
        }
    };
}

/// Host-side problems collected while the device is streaming.
///
/// The transfer must never be aborted for a host-side error: the phone streams the
/// rest of the batch regardless, so bailing out mid-stream kills the connection and
/// loses every file after it. The bytes for the one problem file are dropped instead,
/// the reason is recorded here, and the run reports it at the end.
#[derive(Default)]
struct Failures(Mutex<Vec<String>>);

impl Failures {
    fn push(&self, msg: impl Into<String>) {
        let mut v = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if v.len() < 200 {
            v.push(msg.into());
        }
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// A file the device is writing into, which degrades to discarding instead of failing.
struct SafeFile {
    inner: Option<std::fs::File>,
    path: PathBuf,
    fails: Arc<Failures>,
    broken: bool,
}

impl SafeFile {
    fn mark_failed(&mut self, err: &str) {
        if !self.broken {
            self.broken = true;
            self.fails.push(format!("could not save {}: {err}", self.path.display()));
        }
        // Drop the handle: the remaining bytes of this file go nowhere.
        self.inner = None;
    }
}

impl Write for SafeFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(f) = self.inner.as_mut() else {
            return Ok(buf.len());
        };
        match f.write_all(buf) {
            Ok(()) => Ok(buf.len()),
            Err(e) => {
                let err = e.to_string();
                self.mark_failed(&err);
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.inner.as_mut() {
            if let Err(e) = f.flush() {
                let err = e.to_string();
                self.mark_failed(&err);
            }
        }
        Ok(())
    }
}

impl Drop for SafeFile {
    fn drop(&mut self) {
        let _ = self.flush();
        if self.broken {
            // A half-written blob would restore as corrupt data, and its fileID would
            // stop the retry from fetching it again. Remove it and let the retry refill.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Filesystem delegate that adds real free-space reporting, a progress bar, and
/// error-tolerant file writes.
struct Delegate {
    fs: FsBackupDelegate,
    bar: ProgressBar,
    fails: Arc<Failures>,
    received: AtomicU64,
}

impl Delegate {
    fn new(fails: Arc<Failures>) -> Self {
        let bar = ProgressBar::new(10_000);
        bar.set_style(
            ProgressStyle::with_template(
                "    backup [{bar:30}] {percent:>3}%  {msg}  elapsed {elapsed_precise}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        bar.enable_steady_tick(Duration::from_millis(250));
        bar.set_message("waiting for device…");
        crate::util::track_gauge("device backup", &bar, crate::util::GaugeUnit::Percent);
        Self {
            fs: FsBackupDelegate,
            bar,
            fails,
            received: AtomicU64::new(0),
        }
    }

    /// Number of files received from the device so far.
    fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

/// The part of a path that is meaningful in a report (blob names are hashes anyway).
fn relative(from: &Path, to: &Path) -> String {
    to.strip_prefix(from)
        .ok()
        .or_else(|| to.file_name().map(Path::new))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| to.display().to_string())
}

fn status<T>(r: &Result<T, IdeviceError>) -> &'static str {
    if r.is_ok() {
        "ok"
    } else {
        "FAILED"
    }
}

impl BackupDelegate for Delegate {
    fn get_free_disk_space(&self, path: &Path) -> u64 {
        // The device refuses to back up if this looks too small, so never report 0 by accident.
        crate::util::free_space(path).unwrap_or(1 << 50)
    }
    fn on_progress(&self, p: BackupProgress) {
        if p.overall_progress >= 0.0 {
            self.bar.set_position((p.overall_progress * 100.0).clamp(0.0, 10_000.0) as u64);
        }
        let msg = if p.session_bytes_total > 0 {
            format!(
                "{} / ~{}",
                crate::util::human_bytes(p.session_bytes_done),
                crate::util::human_bytes(p.session_bytes_total)
            )
        } else {
            crate::util::human_bytes(p.session_bytes_done)
        };
        self.bar.set_message(msg);
    }
    fn open_file_read<'a>(&'a self, p: &'a Path) -> Fut<'a, Result<Box<dyn Read + Send>, IdeviceError>> {
        self.fs.open_file_read(p)
    }

    // The upload loop below is deliberately forgiving: a file this computer cannot
    // write (bad name, permission, disk full, path too long) is skipped and reported
    // instead of aborting a transfer that may already be tens of gigabytes in.
    fn create_file_write<'a>(&'a self, p: &'a Path) -> Fut<'a, Result<Box<dyn Write + Send>, IdeviceError>> {
        Box::pin(async move {
            let mut broken = false;
            let inner = match tokio::fs::File::create(p).await {
                Ok(f) => Some(f.into_std().await),
                Err(e) => {
                    trace!("create {} failed: {e}", p.display());
                    broken = true;
                    self.fails.push(format!("could not create {}: {e}", p.display()));
                    None
                }
            };
            let w = SafeFile {
                inner,
                path: p.to_path_buf(),
                fails: self.fails.clone(),
                broken,
            };
            Ok(Box::new(w) as Box<dyn Write + Send>)
        })
    }

    fn create_dir_all<'a>(&'a self, p: &'a Path) -> Fut<'a, Result<(), IdeviceError>> {
        Box::pin(async move {
            if let Err(e) = tokio::fs::create_dir_all(p).await {
                trace!("mkdir {} failed: {e}", p.display());
                self.fails.push(format!("could not create folder {}: {e}", p.display()));
            }
            // Never fail the message loop over this: file creation reports the real problem.
            Ok(())
        })
    }

    fn remove<'a>(&'a self, p: &'a Path) -> Fut<'a, Result<(), IdeviceError>> {
        Box::pin(async move {
            // A file held open by antivirus or an indexer is common on Windows; report it
            // and keep going instead of failing the device's bookkeeping request.
            let r = self.fs.remove(p).await;
            trace!("remove {} -> {}", p.display(), status(&r));
            if let Err(e) = r {
                self.fails.push(format!("could not remove {}: {e}", p.display()));
            }
            Ok(())
        })
    }

    // A failed bookkeeping operation must not be reported to the device. The device
    // sends one MoveFiles message per batch: if any single entry fails, the whole batch
    // is answered with -1, and the phone abandons the backup with error 104 right at the
    // end — after an hour of transfer, with no Manifest.db, which makes the folder
    // unrestorable. So the failure is recorded here and the device is told "done"; the
    // audit at the end of the run names every file that is not actually on disk.
    fn rename<'a>(&'a self, a: &'a Path, b: &'a Path) -> Fut<'a, Result<(), IdeviceError>> {
        Box::pin(async move {
            let r = self.fs.rename(a, b).await;
            trace!("move {} -> {} : {}", a.display(), b.display(), status(&r));
            if let Err(e) = r {
                if self.fs.exists(a).await {
                    self.fails.push(format!(
                        "could not move {} into place: {e}",
                        relative(a, b)
                    ));
                } else {
                    // The phone announced this file but never delivered its bytes, so
                    // there is nothing to move. Not a host problem, and fatal to the
                    // remaining moves if reported.
                    self.fails.push(format!("the iPhone never sent {} ({e})", relative(a, b)));
                }
            }
            Ok(())
        })
    }

    fn copy<'a>(&'a self, a: &'a Path, b: &'a Path) -> Fut<'a, Result<(), IdeviceError>> {
        Box::pin(async move {
            let r = self.fs.copy(a, b).await;
            trace!("copy {} -> {} : {}", a.display(), b.display(), status(&r));
            if let Err(e) = r {
                self.fails.push(format!("could not copy {}: {e}", relative(a, b)));
            }
            Ok(())
        })
    }

    fn exists<'a>(&'a self, p: &'a Path) -> Fut<'a, bool> {
        self.fs.exists(p)
    }

    fn is_dir<'a>(&'a self, p: &'a Path) -> Fut<'a, bool> {
        self.fs.is_dir(p)
    }

    fn list_dir<'a>(&'a self, p: &'a Path) -> Fut<'a, Result<Vec<DirEntryInfo>, IdeviceError>> {
        Box::pin(async move {
            match self.fs.list_dir(p).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    self.fails.push(format!("could not list {}: {e}", p.display()));
                    Ok(Vec::new())
                }
            }
        })
    }

    fn on_file_received(&self, path: &str, file_count: u32) {
        self.received.fetch_add(1, Ordering::Relaxed);
        trace!("received #{file_count} {path}");
    }
}

pub struct BackupResult {
    pub dir: PathBuf,
    pub encrypted: bool,
    /// Files this snapshot is missing (empty when the backup is complete).
    pub errors: Vec<String>,
}

/// Run a backup into `<backup_root>/<UDID>/`. Repeat runs are incremental unless `full`.
pub async fn run(
    dev: &UsbmuxdDevice,
    backup_root: &Path,
    full: bool,
    require_encryption: bool,
) -> Result<BackupResult> {
    let p = device::provider(dev)?;
    let info = device::info(&p).await?;
    let encrypted = device::will_encrypt(&p).await.unwrap_or(false);

    if !encrypted {
        if require_encryption {
            bail!(
                "Backup encryption is off, so Health, Keychain, saved passwords and Wi-Fi \
                 settings would be left out.\n  Turn it on with `ibackup encryption enable`, \
                 or drop --require-encryption."
            );
        }
        util::say(
            "  note: backup encryption is OFF. Health data, Keychain/passwords and Wi-Fi \
             settings are only included in encrypted backups (`ibackup encryption enable`).",
        );
    }

    tokio::fs::create_dir_all(backup_root)
        .await
        .with_context(|| format!("creating {}", backup_root.display()))?;
    preflight_space(backup_root);

    let dir = backup_root.join(&dev.udid);
    util::say("  The iPhone may ask for its passcode to start the backup.");

    let fails = Arc::new(Failures::default());
    let mut errors: Vec<String> = Vec::new();
    let mut last_fatal: Option<anyhow::Error> = None;
    let mut audit = Audit::default();
    let mut complete = false;

    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            util::say("  …trying again: the phone only sends the files still missing");
        }
        // A retry is incremental even after `--full`: re-running a forced full backup of a
        // large phone to recover a handful of files would take another hour.
        let options = if attempt == 1 && full {
            let mut o = Dictionary::new();
            o.insert("ForceFullBackup".into(), Value::Boolean(true));
            Some(o)
        } else {
            None
        };

        let mut client: MobileBackup2Client = match device::connect(&p, "the backup service").await {
            Ok(c) => c,
            Err(e) => {
                last_fatal = Some(e);
                continue;
            }
        };
        let delegate = Delegate::new(fails.clone());
        let res = client
            .backup_from_path(backup_root, Some(&dev.udid), options, &delegate)
            .await;
        let received = delegate.received();
        delegate.bar.finish_and_clear();
        let _ = client.disconnect().await;

        let mut round: Vec<String> = fails.take();
        let mut fatal: Option<anyhow::Error> = None;
        match res {
            Ok(Some(resp)) => {
                let code = resp
                    .get("ErrorCode")
                    .and_then(|v| v.as_unsigned_integer())
                    .unwrap_or(0);
                let desc = resp.get("ErrorDescription").and_then(|v| v.as_string());
                if code == DEVICE_ERR_MULTIPLE_UPLOAD_ERRORS {
                    // The device reports one code for a whole set of per-file failures and
                    // never says which files, so the folder audit below has to find them.
                    round.push(format!(
                        "the iPhone reported per-file errors: {}",
                        desc.unwrap_or("Multiple errors uploading files")
                    ));
                } else if code != 0 {
                    let desc = desc.unwrap_or("unknown error");
                    fatal = Some(anyhow::anyhow!(
                        "Device reported backup error {code}: {desc}{}",
                        hint_for(code)
                    ));
                }
            }
            Ok(None) => {
                fatal = Some(anyhow::anyhow!(
                    "the iPhone ended the backup without reporting a result (cable unplugged, \
                     phone locked, or an app restarted the backup)"
                ));
            }
            Err(e) => fatal = Some(anyhow::anyhow!("Backup failed: {e}")),
        }
        util::say(&format!("  {received} files received this pass"));

        // The folder is the ground truth: whatever the protocol claims, a blob that is not
        // on disk is a missing file, and Manifest.db is what a restore would believe.
        audit = match audit_backup(&dir) {
            Ok(a) => a,
            Err(e) => {
                util::say(&format!("  could not check the backup folder: {e:#}"));
                Audit::default()
            }
        };
        if attempt > 1 && !round.is_empty() {
            util::say(&format!("  {} file errors reported this pass", round.len()));
        }
        errors = round;
        let failed_hard = fatal.is_some();
        if let Some(e) = &fatal {
            errors.insert(0, format!("{e:#}"));
        }
        last_fatal = fatal;
        describe_gaps(&audit, &mut errors);

        if !failed_hard && errors.is_empty() && audit.snapshot_finished {
            complete = true;
            break;
        }
        if !audit.has_manifest {
            // Nothing was committed: the phone writes Manifest.db last, and without it a
            // retry would transfer the whole phone again. Report instead of burning hours.
            last_fatal = last_fatal.or_else(|| {
                Some(anyhow::anyhow!(
                    "the iPhone did not finish, so it never wrote Manifest.db — an unfinished \
                     backup cannot be restored"
                ))
            });
            break;
        }
    }

    if !audit.has_manifest {
        let why = last_fatal
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "the iPhone did not write a manifest".into());
        bail!("No usable backup was written to {} — {why}", dir.display());
    }

    // Host metadata, so Finder / Apple Devices / `ibackup browse` list the backup.
    write_info_plist(&dir, &info, &dev.udid).context("writing Info.plist")?;

    if complete && errors.is_empty() {
        verify(&dir)?;
    } else {
        util::say("");
        if audit.missing_total > 0 {
            util::say(&format!(
                "  The backup in this folder is usable, but {} of {} files are missing from it.",
                audit.missing_total, audit.files_total
            ));
            util::say(
                "  Those are files the phone refused to hand over or that could not be saved here.",
            );
            util::say(
                "  Usual causes: a file in use by an app on the phone, a failing flash chip on the",
            );
            util::say(
                "  phone, or this computer failing to write (disk space, permissions, antivirus).",
            );
            util::say(
                "  Running the same command again is quick: only the missing files are fetched.",
            );
        } else if !audit.snapshot_finished {
            util::say("  Every file is there, but the phone never marked the backup finished");
            util::say("  (Status.plist). Running the same command again usually settles it.");
        }
    }

    Ok(BackupResult {
        dir,
        encrypted,
        errors,
    })
}

/// What to do about the device errors that have a known cure.
fn hint_for(code: u64) -> &'static str {
    match code {
        103 => "\n  The phone could not read or write this computer's backup folder. Check free \
                space and permissions on the destination.",
        208 => "\n  Unlock the iPhone and leave it unlocked while the backup runs \
                (Settings -> Display & Brightness -> Auto-Lock -> Never).",
        105 | 106 => "\n  The phone is busy or out of space for the backup. Close apps that are \
                     writing, then try again.",
        _ => "",
    }
}

/// Fold the folder's own verdict into the error list.
fn describe_gaps(audit: &Audit, errors: &mut Vec<String>) {
    if audit.missing_total > 0 {
        for m in &audit.missing {
            errors.push(format!("missing from this backup: {m}"));
        }
        if audit.missing_total as usize > audit.missing.len() {
            errors.push(format!(
                "…and {} more files missing from this backup",
                audit.missing_total as usize - audit.missing.len()
            ));
        }
    }
    if !audit.snapshot_finished {
        errors.push("the iPhone did not mark this snapshot finished (Status.plist)".into());
    }
}

/// Warn before a long transfer when the destination cannot possibly hold it.
fn preflight_space(backup_root: &Path) {
    let Some(free) = util::free_space(backup_root) else {
        return;
    };
    util::say(&format!("  destination has {} free", util::human_bytes(free)));
    if free < (1u64 << 30) {
        util::say(&format!(
            "  warning: only {} free. A full backup needs about as much space as the phone is \
             using, and a phone that fills the disk mid-way fails with confusing errors.",
            util::human_bytes(free)
        ));
    }
}

/// What the backup folder actually contains, according to its own Manifest.db.
#[derive(Default)]
struct Audit {
    has_manifest: bool,
    snapshot_finished: bool,
    files_total: u64,
    /// Manifest entries whose file blob is not on disk (first `REPORT_LIMIT` names).
    missing: Vec<String>,
    missing_total: u64,
}

/// Cross-check Manifest.db against the blobs actually written to the folder.
/// This is what turns "error 104" into an actionable list of files.
fn audit_backup(dir: &Path) -> Result<Audit> {
    let mut a = Audit::default();

    a.snapshot_finished = plist::from_file::<_, Dictionary>(dir.join("Status.plist"))
        .ok()
        .and_then(|s| {
            s.get("SnapshotState")
                .and_then(|v| v.as_string())
                .map(|v| v == "finished")
        })
        .unwrap_or(false);

    let db_path = dir.join("Manifest.db");
    if !db_path.is_file() {
        return Ok(a);
    }
    a.has_manifest = true;

    let db = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db_path.display()))?;
    // flags: 1 = file, 2 = directory, 4 = symlink
    let mut q = db.prepare("SELECT fileID, domain, relativePath FROM Files WHERE flags = 1")?;
    let mut rows = q.query([])?;
    // A manifest that cannot be read to the end still tells us a lot, so a bad row
    // is skipped rather than losing the whole audit.
    while let Ok(Some(r)) = rows.next() {
        let Some(id) = r.get::<_, Option<String>>(0).ok().flatten() else {
            continue;
        };
        let domain = r.get::<_, Option<String>>(1).ok().flatten().unwrap_or_default();
        let rel = r.get::<_, Option<String>>(2).ok().flatten().unwrap_or_default();
        a.files_total += 1;
        if !blob_path(dir, &id).is_file() {
            a.missing_total += 1;
            if a.missing.len() < REPORT_LIMIT {
                a.missing.push(format!("{domain}/{rel}"));
            }
        }
    }
    Ok(a)
}

/// Blobs live at <backup>/<first two hex chars>/<fileID>.
fn blob_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(id.get(..2).unwrap_or("")).join(id)
}

/// Sanity-check that the device left a complete backup behind.
fn verify(dir: &Path) -> Result<()> {
    for f in ["Manifest.plist", "Manifest.db", "Status.plist"] {
        if !dir.join(f).exists() {
            bail!(
                "Backup looks incomplete: {} is missing",
                dir.join(f).display()
            );
        }
    }
    let status: Dictionary =
        plist::from_file(dir.join("Status.plist")).context("reading Status.plist")?;
    if status.get("SnapshotState").and_then(|v| v.as_string()) != Some("finished") {
        bail!("Backup did not finish (Status.plist SnapshotState is not \"finished\")");
    }
    Ok(())
}

/// Info.plist is host-written metadata. Finder/iTunes use it to list the backup.
fn write_info_plist(dir: &Path, info: &Dictionary, udid: &str) -> Result<()> {
    let s = |k: &str| info.get(k).cloned();
    let mut d = Dictionary::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            d.insert(k.into(), v);
        }
    };
    put("Build Version", s("BuildVersion"));
    put("Device Name", s("DeviceName"));
    put("Display Name", s("DeviceName"));
    put("GUID", Some(Value::String(uuid::Uuid::new_v4().simple().to_string().to_uppercase())));
    put("ICCID", s("IntegratedCircuitCardIdentity"));
    put("IMEI", s("InternationalMobileEquipmentIdentity"));
    put("MEID", s("MobileEquipmentIdentifier"));
    put("Phone Number", s("PhoneNumber"));
    put("Last Backup Date", Some(Value::Date(SystemTime::now().into())));
    put("Product Name", s("DeviceClass"));
    put("Product Type", s("ProductType"));
    put("Product Version", s("ProductVersion"));
    put("Serial Number", s("SerialNumber"));
    put("Target Identifier", Some(Value::String(udid.to_string())));
    put("Target Type", Some(Value::String("Device".into())));
    put("Unique Identifier", Some(Value::String(udid.to_uppercase())));
    put("iTunes Version", Some(Value::String("12.13".into())));
    plist::to_file_xml(dir.join("Info.plist"), &Value::Dictionary(d))?;
    Ok(())
}

pub enum EncryptionAction {
    Status,
    Enable,
    Disable,
    ChangePassword,
}

fn read_password(prompt: &str) -> Result<String> {
    if let Ok(pw) = std::env::var("IBACKUP_PASSWORD") {
        return Ok(pw);
    }
    Ok(rpassword::prompt_password(prompt)?)
}

fn new_password() -> Result<String> {
    if let Ok(pw) = std::env::var("IBACKUP_PASSWORD") {
        return Ok(pw);
    }
    let a = rpassword::prompt_password("New backup password: ")?;
    let b = rpassword::prompt_password("Repeat password: ")?;
    if a != b {
        bail!("Passwords do not match");
    }
    if a.is_empty() {
        bail!("Password cannot be empty");
    }
    Ok(a)
}

pub async fn encryption(dev: &UsbmuxdDevice, action: EncryptionAction) -> Result<()> {
    let p = device::provider(dev)?;
    let on = device::will_encrypt(&p).await?;
    let (old, new) = match action {
        EncryptionAction::Status => {
            println!("Backup encryption is {}", if on { "ON" } else { "OFF" });
            return Ok(());
        }
        EncryptionAction::Enable => {
            if on {
                println!("Backup encryption is already ON.");
                return Ok(());
            }
            crate::util::say("Keep this password safe: without it the backup cannot be restored or read.");
            (None, Some(new_password()?))
        }
        EncryptionAction::Disable => {
            if !on {
                println!("Backup encryption is already OFF.");
                return Ok(());
            }
            (Some(read_password("Current backup password: ")?), None)
        }
        EncryptionAction::ChangePassword => {
            if !on {
                bail!("Encryption is OFF; use `ibackup encryption enable` instead.");
            }
            (Some(read_password("Current backup password: ")?), Some(new_password()?))
        }
    };

    let scratch = std::env::temp_dir().join(format!("ibackup-{}", std::process::id()));
    tokio::fs::create_dir_all(&scratch).await?;
    let mut client: MobileBackup2Client = device::connect(&p, "the backup service").await?;
    crate::util::say("Confirm on the iPhone (it will ask for the device passcode)…");
    let r = client
        .change_password_from_path(&scratch, old.as_deref(), new.as_deref(), &FsBackupDelegate)
        .await;
    let _ = client.disconnect().await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    r.context("the device rejected the change (wrong password, or passcode not entered?)")?;

    let now = device::will_encrypt(&p).await.unwrap_or(on);
    println!("Backup encryption is now {}", if now { "ON" } else { "OFF" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ibackup-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unwritable_file_is_recorded_not_fatal() {
        // Exactly the state `create_file_write` falls back to when a path cannot be
        // created: writes must be accepted (and dropped) so the device keeps streaming.
        let dir = scratch("safefile");
        let fails = Arc::new(Failures::default());
        let path = dir.join("no-such-dir").join("blob");
        let mut w = SafeFile {
            inner: None,
            path: path.clone(),
            fails: fails.clone(),
            broken: false,
        };
        assert_eq!(w.write(b"0123456789").unwrap(), 10);
        drop(w);
        assert!(!path.exists());
        assert!(fails.take().is_empty(), "no error was ever recorded");
    }

    #[tokio::test]
    async fn a_move_the_host_cannot_do_is_not_reported_to_the_device() {
        // The device sends one MoveFiles message per batch and the crate answers -1 for
        // the whole batch as soon as one rename fails — which abandons the backup with
        // error 104 at the very end, after an hour of transfer, with no Manifest.db.
        // A phone that announces a file and never delivers its bytes must therefore be
        // recorded, not reported.
        let dir = scratch("move");
        let fails = Arc::new(Failures::default());
        let delegate = Delegate::new(fails.clone());
        let staging = dir.join("Snapshot").join("aa").join("aa11");
        let target = dir.join("aa").join("aa11");
        std::fs::create_dir_all(dir.join("aa")).unwrap();

        let r = delegate.rename(&staging, &target).await;
        assert!(r.is_ok(), "one missing file must not answer -1 to the batch");
        let seen = fails.take();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("never sent"), "{seen:?}");
    }

    #[test]
    fn audit_names_blobs_missing_from_the_folder() {
        let dir = scratch("audit");
        let present = "aa11";
        std::fs::create_dir_all(dir.join("aa")).unwrap();
        std::fs::write(dir.join("aa").join(present), b"x").unwrap();

        let db = Connection::open(dir.join("Manifest.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE Files (fileID TEXT, domain TEXT, relativePath TEXT, flags INTEGER);
             INSERT INTO Files VALUES ('aa11', 'CameraRollDomain', 'Media/DCIM/a.JPG', 1);
             INSERT INTO Files VALUES ('bb22', 'CameraRollDomain', 'Media/DCIM/b.JPG', 1);
             INSERT INTO Files VALUES ('cc33', 'CameraRollDomain', 'Media/DCIM', 2);",
        )
        .unwrap();
        db.close().unwrap();

        let a = audit_backup(&dir).unwrap();
        assert!(a.has_manifest);
        assert!(!a.snapshot_finished, "no Status.plist yet");
        assert_eq!(a.files_total, 2, "directories are not files");
        assert_eq!(a.missing_total, 1);
        assert_eq!(a.missing, vec!["CameraRollDomain/Media/DCIM/b.JPG"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gaps_are_named_and_capped() {
        let mut a = Audit {
            has_manifest: true,
            snapshot_finished: false,
            files_total: 100,
            missing: (0..REPORT_LIMIT)
                .map(|i| format!("CameraRollDomain/Media/DCIM/{i}.JPG"))
                .collect(),
            missing_total: REPORT_LIMIT as u64 + 5,
        };
        let mut errors = Vec::new();
        describe_gaps(&a, &mut errors);
        assert_eq!(
            errors[0],
            "missing from this backup: CameraRollDomain/Media/DCIM/0.JPG"
        );
        assert_eq!(
            errors[REPORT_LIMIT],
            "…and 5 more files missing from this backup"
        );
        assert_eq!(errors.len(), REPORT_LIMIT + 2, "the unfinished snapshot too");

        a.missing_total = 0;
        a.missing.clear();
        a.snapshot_finished = true;
        let mut clean = Vec::new();
        describe_gaps(&a, &mut clean);
        assert!(clean.is_empty());
    }

    #[test]
    fn audit_without_manifest_reports_nothing_to_restore() {
        let dir = scratch("audit-empty");
        let a = audit_backup(&dir).unwrap();
        assert!(!a.has_manifest);
        assert_eq!(a.missing_total, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
