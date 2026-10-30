mod afc_copy;
mod app_docs;
mod backup;
mod device;
mod export;
mod icloud;
mod media;
mod util;
mod prune;
mod verify;
#[cfg(feature = "gui")]
mod browse;
#[cfg(feature = "gui")]
mod ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value as Json};

use util::Stats;

#[derive(Parser)]
#[command(
    name = "ibackup",
    version,
    about = "Back up an iPhone over USB to this computer (Windows, macOS or Linux)",
    long_about = "Back up an iPhone over USB: a full Finder/iTunes-compatible device backup, \
                  readable copies of photos, videos and app documents, and the iCloud data that \
                  this computer syncs. Start with `ibackup all --out <folder>`."
)]
struct Cli {
    /// Which iPhone to use (UDID from `ibackup devices`). Optional when only one is plugged in.
    #[arg(long, global = true, env = "IBACKUP_UDID")]
    udid: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List iPhones connected over USB
    Devices,
    /// Pair with the iPhone ("Trust This Computer"). Done automatically when needed.
    Pair,
    /// Everything: device backup, media, app documents and iCloud, into one folder per device
    All(AllArgs),
    /// Full device backup (Finder/iTunes format, restorable). Incremental on repeat runs.
    Backup {
        /// Backup root; data goes to <out>/<UDID>/
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        b: BackupFlags,
    },
    /// Readable copy of photos and videos (or the whole media partition with --everything)
    Media {
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        m: MediaFlags,
    },
    /// Readable copy of each app's shared Documents folder
    AppDocs {
        #[arg(long)]
        out: PathBuf,
        /// Only these bundle IDs (repeatable)
        #[arg(long = "app")]
        apps: Vec<String>,
    },
    /// Copy iCloud Drive / iCloud Photos as synced to this computer (iCloud is not reachable over USB)
    Icloud {
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        i: IcloudFlags,
    },
    /// Open the desktop window (same features as `all`)
    #[cfg(feature = "gui")]
    Ui,
    /// Explore a finished backup: browse, preview and extract files
    #[cfg(feature = "gui")]
    #[command(visible_alias = "explore")]
    Browse {
        /// The <UDID> backup folder, a device folder, or a parent folder with backups
        backup: Option<PathBuf>,
    },
    /// Show or change backup encryption (needed for Health, Keychain, passwords, Wi-Fi)
    Encryption {
        #[arg(value_enum)]
        action: EncAction,
    },
    /// Turn an unencrypted backup into normal, browsable folders
    Export {
        /// The <UDID> folder made by `backup`
        #[arg(long)]
        backup: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Only domains containing this text, e.g. CameraRollDomain, AppDomain-com.whatsapp (repeatable)
        #[arg(long)]
        domain: Vec<String>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum EncAction {
    Status,
    Enable,
    Disable,
    ChangePassword,
}

#[derive(Args, Clone)]
struct BackupFlags {
    /// Force a full backup instead of incremental
    #[arg(long)]
    full: bool,
    /// Refuse to run unless backup encryption is on (so nothing is left out)
    #[arg(long)]
    require_encryption: bool,
}

#[derive(Args, Clone)]
struct MediaFlags {
    /// Copy the entire media partition (Downloads, Books, Recordings, …), not just photos
    #[arg(long)]
    everything: bool,
    /// Skip device paths starting with this, e.g. "DCIM/100APPLE" (repeatable)
    #[arg(long)]
    exclude: Vec<String>,
    /// After copying, check every file against a hash computed by the iPhone itself
    /// (re-copies any mismatch; results kept in verified.json)
    #[arg(long)]
    verify: bool,
    /// After verifying, DELETE verified camera-roll photos/videos from the iPhone (asks to confirm).
    /// Turn iCloud Photos off first. Implies --verify.
    #[arg(long)]
    delete_verified: bool,
    /// Delete EVERY camera-roll photo/video that has a copy on this computer, verified or not
    /// (asks to confirm). Use when the iPhone itself cannot hash its files. Turn iCloud Photos
    /// off first. Implies --delete-verified but skips the verification pass.
    #[arg(long)]
    delete_all: bool,
    /// With --delete-verified: only delete items dated before this day (YYYY-MM-DD)
    #[arg(long, value_name = "DATE")]
    delete_before: Option<String>,
    /// With --delete-verified: also reset the Photos database so the iPhone rebuilds it
    /// (fixes blank thumbnails; loses albums/favorites). Restart the iPhone afterwards.
    #[arg(long)]
    rebuild_photos_db: bool,
    /// Skip the typed confirmation for --delete-verified
    #[arg(long)]
    yes: bool,
}

impl MediaFlags {
    fn prune(&self) -> Result<Option<prune::Options>> {
        if !self.delete_verified && !self.delete_all {
            for (set, flag) in [
                (self.delete_before.is_some(), "--delete-before"),
                (self.rebuild_photos_db, "--rebuild-photos-db"),
                (self.yes, "--yes"),
            ] {
                if set {
                    bail!("{flag} needs --delete-verified or --delete-all");
                }
            }
            return Ok(None);
        }
        Ok(Some(prune::Options {
            before: self.delete_before.as_deref().map(prune::parse_date).transpose()?,
            yes: self.yes,
            rebuild_db: self.rebuild_photos_db,
            all: self.delete_all,
        }))
    }
}

#[derive(Args, Clone)]
struct IcloudFlags {
    /// Extra local folder to treat as iCloud data (repeatable)
    #[arg(long)]
    source: Vec<PathBuf>,
    /// Don't look for the standard iCloud folders
    #[arg(long)]
    no_auto_detect: bool,
    /// macOS: ask iCloud to download cloud-only files first (needs free disk space)
    #[arg(long)]
    download: bool,
    /// Also download the full iCloud Photos library with icloudpd, signing in as this Apple ID
    #[arg(long, value_name = "APPLE_ID")]
    icloudpd: Option<String>,
    /// Also copy iCloud Drive with rclone from this remote (type iclouddrive; best option on Linux)
    #[arg(long, value_name = "REMOTE")]
    rclone: Option<String>,
}

#[derive(Args)]
struct AllArgs {
    /// Destination folder; each device gets its own subfolder
    #[arg(long)]
    out: PathBuf,
    #[command(flatten)]
    b: BackupFlags,
    #[command(flatten)]
    m: MediaFlags,
    #[command(flatten)]
    i: IcloudFlags,
    /// Skip the full device backup
    #[arg(long)]
    skip_backup: bool,
    /// Skip copying photos/videos/media
    #[arg(long)]
    skip_media: bool,
    /// Skip copying app Documents folders
    #[arg(long)]
    skip_app_docs: bool,
    /// Skip copying iCloud folders
    #[arg(long)]
    skip_icloud: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2), // finished, but with errors
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn icloud_opts(i: &IcloudFlags) -> icloud::Options {
    icloud::Options {
        extra_sources: i.source.clone(),
        no_auto_detect: i.no_auto_detect,
        download: i.download,
        icloudpd_user: i.icloudpd.clone(),
        rclone_remote: i.rclone.clone(),
    }
}

fn print_stats(stage: &str, s: &Stats) -> bool {
    println!("{stage}: {}", s.summary());
    for n in &s.notes {
        println!("  note: {n}");
    }
    for e in s.errors.iter().take(10) {
        println!("  error: {e}");
    }
    if s.errors.len() > 10 {
        println!("  … {} more errors (see report.json)", s.errors.len() - 10);
    }
    s.errors.is_empty()
}

async fn run(cli: Cli) -> Result<bool> {
    let udid = cli.udid.as_deref();
    match cli.cmd {
        Cmd::Devices => device::print_devices().await.map(|_| true),
        Cmd::Pair => {
            let d = device::select(udid).await?;
            device::pair(&d).await.map(|_| true)
        }
        Cmd::Backup { out, b } => {
            let d = device::select(udid).await?;
            device::ensure_paired(&d).await?;
            let r = backup::run(&d, &out, b.full, b.require_encryption).await?;
            println!(
                "Backup {} ({}): {}",
                if r.errors.is_empty() {
                    "complete"
                } else {
                    "finished, but incomplete"
                },
                if r.encrypted { "encrypted" } else { "not encrypted" },
                r.dir.display()
            );
            for e in r.errors.iter().take(20) {
                println!("  error: {e}");
            }
            if r.errors.len() > 20 {
                println!("  … {} more (see report.json)", r.errors.len() - 20);
            }
            Ok(r.errors.is_empty())
        }
        Cmd::Media { out, m } => {
            let d = device::select(udid).await?;
            device::ensure_paired(&d).await?;
            let pr = m.prune()?;
            let s = media::run(&d, &out, m.everything, &m.exclude, m.verify, pr.as_ref()).await?;
            Ok(print_stats("media", &s))
        }
        Cmd::AppDocs { out, apps } => {
            let d = device::select(udid).await?;
            device::ensure_paired(&d).await?;
            let s = app_docs::run(&d, &out, &apps).await?;
            Ok(print_stats("app documents", &s))
        }
        Cmd::Icloud { out, i } => {
            let s = icloud::run(&out, &icloud_opts(&i)).await?;
            Ok(print_stats("icloud", &s))
        }
        Cmd::Encryption { action } => {
            let d = device::select(udid).await?;
            device::ensure_paired(&d).await?;
            let a = match action {
                EncAction::Status => backup::EncryptionAction::Status,
                EncAction::Enable => backup::EncryptionAction::Enable,
                EncAction::Disable => backup::EncryptionAction::Disable,
                EncAction::ChangePassword => backup::EncryptionAction::ChangePassword,
            };
            backup::encryption(&d, a).await.map(|_| true)
        }
        Cmd::Export { backup, out, domain } => {
            let s = tokio::task::spawn_blocking(move || export::run(&backup, &out, &domain)).await??;
            Ok(print_stats("export", &s))
        }
        #[cfg(feature = "gui")]
        Cmd::Ui => crate::ui::run().map(|_| true),
        #[cfg(feature = "gui")]
        Cmd::Browse { backup } => crate::browse::run(backup).map(|_| true),
        Cmd::All(a) => run_all(udid, a).await,
    }
}

fn stage_json(r: &Result<Stats>) -> Json {
    match r {
        Ok(s) => json!({ "ok": s.errors.is_empty(), "stats": s }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }
}

async fn run_all(udid: Option<&str>, a: AllArgs) -> Result<bool> {
    let started = chrono::Local::now();
    let d = device::select(udid).await?;
    device::ensure_paired(&d).await?;
    let p = device::provider(&d)?;
    let info = device::info(&p).await?;
    let base = a.out.join(device::folder_name(&info, &d.udid));
    std::fs::create_dir_all(&base)?;

    println!(
        "Backing up {} ({}, iOS {}) to {}",
        device::get_str(&info, "DeviceName"),
        device::get_str(&info, "ProductType"),
        device::get_str(&info, "ProductVersion"),
        base.display()
    );
    let mut report = json!({
        "tool": concat!("ibackup ", env!("CARGO_PKG_VERSION")),
        "started": started.to_rfc3339(),
        "device": {
            "udid": d.udid,
            "name": device::get_str(&info, "DeviceName"),
            "model": device::get_str(&info, "ProductType"),
            "ios": device::get_str(&info, "ProductVersion"),
        },
        "stages": {},
    });
    let mut all_ok = true;
    let mut record = |name: &str, r: Result<Stats>, report: &mut Json| {
        match &r {
            Ok(s) => all_ok &= print_stats(name, s),
            Err(e) => {
                println!("{name}: FAILED: {e:#}");
                all_ok = false;
            }
        }
        report["stages"][name] = stage_json(&r);
    };

    if !a.skip_backup {
        println!("\n[1/4] Full device backup");
        let r = backup::run(&d, &base.join("backup"), a.b.full, a.b.require_encryption)
            .await
            .map(|res| {
                let mut s = Stats::default();
                s.errors = res.errors;
                s.notes.push(format!(
                    "{} backup at {}",
                    if res.encrypted { "encrypted" } else { "unencrypted" },
                    res.dir.display()
                ));
                s
            });
        record("device-backup", r, &mut report);
    }
    if !a.skip_media {
        println!("\n[2/4] Photos, videos & media");
        let r = match a.m.prune() {
            Ok(pr) => media::run(&d, &base.join("media"), a.m.everything, &a.m.exclude, a.m.verify, pr.as_ref()).await,
            Err(e) => Err(e),
        };
        record("media", r, &mut report);
    }
    if !a.skip_app_docs {
        println!("\n[3/4] App documents");
        let r = app_docs::run(&d, &base.join("app-documents"), &[]).await;
        record("app-documents", r, &mut report);
    }
    if !a.skip_icloud {
        println!("\n[4/4] iCloud (from this computer's iCloud sync)");
        let r = icloud::run(&base.join("icloud"), &icloud_opts(&a.i)).await;
        record("icloud", r, &mut report);
    }

    report["finished"] = json!(chrono::Local::now().to_rfc3339());
    report["ok"] = json!(all_ok);
    write_report(&base, &report)?;
    println!(
        "\n{} Report: {}",
        if all_ok { "Done." } else { "Done, with errors." },
        base.join("report.json").display()
    );
    Ok(all_ok)
}

fn write_report(base: &Path, report: &Json) -> Result<()> {
    std::fs::write(base.join("report.json"), serde_json::to_vec_pretty(report)?)?;
    Ok(())
}
