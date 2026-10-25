//! `ibackup ui` — the desktop window around the same engine as the CLI.
//!
//! The window is deliberately thin: a worker thread owns the engine futures,
//! [`UiCore`] holds the shared snapshot behind one mutex, and the egui thread
//! only draws that snapshot. All styling lives in [`theme`], so the layout
//! below reads as content, not colours.
//!
//! Layout: a slim header with the live status, a plan panel on the right with
//! the main call to action, and the work area on the left (device, destination,
//! stages, progress, activity).

pub(crate) mod theme;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Align, Color32, Layout, RichText, Rounding, Sense, Vec2};
use futures::future::BoxFuture;
use serde_json::{json, Value as Json};

use crate::backup::EncryptionAction;
use crate::util::{human_bytes, Gauge, GaugeUnit, Stats};
use crate::{app_docs, backup, device, icloud, media, util};
use theme as t;

const GB: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Shared state between the UI thread and the worker thread
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DeviceEntry {
    udid: String,
    name: String,
    model: String,
    ios: String,
    paired: bool,
    encrypted: Option<bool>,
}

impl DeviceEntry {
    fn new_unpaired(udid: String) -> Self {
        Self {
            udid,
            name: String::new(),
            model: String::new(),
            ios: String::new(),
            paired: false,
            encrypted: None,
        }
    }
}

fn short_udid(udid: &str) -> String {
    let s: String = udid.chars().take(16).collect();
    if udid.chars().count() > 16 {
        format!("{s}…")
    } else {
        s
    }
}

#[derive(Clone, Copy, PartialEq)]
enum StageStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Clone)]
struct Stage {
    key: &'static str,
    title: &'static str,
    status: StageStatus,
    lines: Vec<String>,
}

#[derive(Clone, Default)]
struct Job {
    started: Option<Instant>,
    stages: Vec<Stage>,
    /// Where this job writes, once the destination folder is known.
    out: Option<PathBuf>,
    finished: Option<String>,
    ok: bool,
}

type Shared = Arc<Mutex<UiCore>>;

struct UiCore {
    ctx: egui::Context,
    tx: Option<Sender<Task>>,
    busy: Option<String>,
    devices: Vec<DeviceEntry>,
    devices_error: Option<String>,
    job: Job,
}

fn with_core(core: &Shared, f: impl FnOnce(&mut UiCore)) {
    let mut c = core.lock().unwrap_or_else(|p| p.into_inner());
    f(&mut c);
    c.ctx.request_repaint();
}

// ---------------------------------------------------------------------------
// Tasks the UI hands to the worker thread (one at a time)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Options {
    out: PathBuf,
    backup: bool,
    full: bool,
    require_enc: bool,
    media: bool,
    everything: bool,
    verify: bool,
    appdocs: bool,
    icloud: bool,
    icloud_download: bool,
    icloudpd: Option<String>,
}

enum Task {
    Refresh,
    Pair(String),
    Enc {
        udid: String,
        enable: bool,
        password: String,
    },
    Run {
        udid: String,
        options: Options,
    },
}

impl Task {
    /// Short enough to sit in the header as a status chip.
    fn busy_label(&self) -> &'static str {
        match self {
            Task::Refresh => "Looking for iPhone",
            Task::Pair(_) => "Waiting for trust",
            Task::Enc { .. } => "Changing encryption",
            Task::Run { .. } => "Backing up",
        }
    }
}

fn worker_loop(rx: Receiver<Task>, core: Shared) {
    // One persistent multi-thread runtime is reused by all tasks.
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            util::say(&format!("could not start the GUI worker: {e}"));
            return;
        }
    };

    while let Some((fut, label)) = rx.recv().ok().map(|task| {
        let label = task.busy_label().to_string();
        let fut: BoxFuture<'_, Result<()>> = match task {
            Task::Refresh => Box::pin(refresh(core.clone())),
            Task::Pair(u) => Box::pin(pair(core.clone(), u)),
            Task::Enc {
                udid,
                enable,
                password,
            } => Box::pin(encryption(core.clone(), udid, enable, password)),
            Task::Run { udid, options } => Box::pin(run_job(core.clone(), udid, options)),
        };
        (fut, label)
    }) {
        with_core(&core, |c| c.busy = Some(label));
        if let Err(e) = rt.block_on(fut) {
            util::say(&format!("error: {e:#}"));
            with_core(&core, |c| {
                // A failed task must not leave a stage spinning "Running".
                for s in c.job.stages.iter_mut() {
                    if s.status == StageStatus::Running {
                        s.status = StageStatus::Failed;
                    }
                }
                c.job.finished.get_or_insert_with(|| format!("Failed: {e:#}"));
            });
        }
        with_core(&core, |c| c.busy = None);
    }
}

async fn refresh(core: Shared) -> Result<()> {
    let res: Result<Vec<DeviceEntry>> = async {
        let devs = device::usb_devices().await?;
        let mut list = Vec::new();
        for d in &devs {
            let p = device::provider(d)?;
            let info = device::info(&p).await.ok();
            let encrypted = device::will_encrypt(&p).await.ok();
            let mut e = DeviceEntry::new_unpaired(d.udid.clone());
            if let Some(i) = info {
                e.name = device::get_str(&i, "DeviceName").to_string();
                e.model = device::get_str(&i, "ProductType").to_string();
                e.ios = device::get_str(&i, "ProductVersion").to_string();
                e.paired = true;
            }
            e.encrypted = encrypted;
            list.push(e);
        }
        Ok(list)
    }
    .await;

    match res {
        Ok(list) => {
            util::say(&format!("Found {} iPhone(s) over USB.", list.len()));
            with_core(&core, |c| {
                c.devices = list;
                c.devices_error = None;
            });
        }
        Err(e) => {
            with_core(&core, |c| c.devices_error = Some(format!("{e:#}")));
        }
    }
    Ok(())
}

async fn pair(core: Shared, udid: String) -> Result<()> {
    util::say("Unlock the iPhone and tap \"Trust This Computer\", then enter your passcode…");
    let d = device::select(Some(&udid)).await?;
    let r = device::ensure_paired(&d).await;
    match r {
        Ok(()) => {
            util::say("Paired. Keep the iPhone unlocked while backups copy photos.");
            with_core(&core, |c| {
                for e in c.devices.iter_mut() {
                    if e.udid == udid {
                        e.paired = true;
                    }
                }
            });
            worker_send(core.clone(), Task::Refresh);
            Ok(())
        }
        Err(e) => {
            with_core(&core, |c| {
                for entry in c.devices.iter_mut() {
                    if entry.udid == udid {
                        entry.paired = false;
                    }
                }
            });
            Err(e)
        }
    }
}

fn worker_send(core: Shared, task: Task) {
    // `tx` goes None once the window is gone: the worker then sees the channel
    // closed and can be joined instead of keeping the process alive.
    let sender = core.lock().unwrap_or_else(|p| p.into_inner()).tx.clone();
    if let Some(tx) = sender {
        let _ = tx.send(task);
    }
}

async fn encryption(core: Shared, udid: String, enable: bool, password: String) -> Result<()> {
    let d = device::select(Some(&udid)).await?;
    // backup::encryption reads the password from IBACKUP_PASSWORD, so the GUI
    // never needs a terminal password prompt.
    std::env::set_var("IBACKUP_PASSWORD", &password);
    if enable {
        util::say("Keep this password safe: an encrypted backup cannot be read without it.");
    }
    let action = if enable {
        EncryptionAction::Enable
    } else {
        EncryptionAction::Disable
    };
    let r = backup::encryption(&d, action).await;
    std::env::remove_var("IBACKUP_PASSWORD");
    if let Ok(p) = device::provider(&d) {
        let now = device::will_encrypt(&p).await.ok();
        with_core(&core, |c| {
            for e in c.devices.iter_mut() {
                if e.udid == udid {
                    e.encrypted = now;
                }
            }
        });
    }
    r
}

// ---------------------------------------------------------------------------
// The backup job: the same stages as `ibackup all`
// ---------------------------------------------------------------------------

fn selected_stages(opts: &Options) -> Vec<Stage> {
    let mut v = Vec::new();
    if opts.backup {
        v.push(Stage {
            key: "device-backup",
            title: "Full device backup",
            status: StageStatus::Pending,
            lines: vec![],
        });
    }
    if opts.media {
        v.push(Stage {
            key: "media",
            title: "Photos & videos",
            status: StageStatus::Pending,
            lines: vec![],
        });
    }
    if opts.appdocs {
        v.push(Stage {
            key: "app-documents",
            title: "App documents",
            status: StageStatus::Pending,
            lines: vec![],
        });
    }
    if opts.icloud {
        v.push(Stage {
            key: "icloud",
            title: "iCloud folders",
            status: StageStatus::Pending,
            lines: vec![],
        });
    }
    v
}

fn stage_set(core: &Shared, key: &str, status: StageStatus, lines: &[String]) {
    with_core(core, |c| {
        for s in c.job.stages.iter_mut() {
            if s.key == key {
                s.status = status;
                s.lines.extend(lines.iter().cloned());
            }
        }
    });
}

fn stage_summary(s: &Stats) -> Vec<String> {
    let mut v = vec![s.summary()];
    for n in &s.notes {
        v.push(format!("note: {n}"));
    }
    for e in s.errors.iter().take(10) {
        v.push(format!("error: {e}"));
    }
    if s.errors.len() > 10 {
        v.push(format!(
            "… {} more errors (details in report.json)",
            s.errors.len() - 10
        ));
    }
    v
}

fn stage_json(s: &Result<Stats>) -> Json {
    match s {
        Ok(st) => json!({"ok": st.errors.is_empty(), "stats": st}),
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    }
}

async fn run_job(core: Shared, udid: String, opts: Options) -> Result<()> {
    with_core(&core, |c| {
        c.job = Job {
            started: Some(Instant::now()),
            stages: selected_stages(&opts),
            out: None,
            finished: None,
            ok: false,
        };
    });

    // Shared setup: device, pairing, per-device destination folder.
    let setup = async {
        let d = device::select(Some(&udid)).await?;
        device::ensure_paired(&d).await?;
        let p = device::provider(&d)?;
        let info = device::info(&p).await?;
        let base = opts.out.join(device::folder_name(&info, &d.udid));
        tokio::fs::create_dir_all(&base)
            .await
            .map_err(|e| anyhow::anyhow!("creating {}: {e}", base.display()))?;
        util::say(&format!(
            "Backing up {} (iOS {}) to {}",
            device::get_str(&info, "DeviceName"),
            device::get_str(&info, "ProductVersion"),
            base.display()
        ));
        Ok::<_, anyhow::Error>((d, base))
    }
    .await;

    let (d, base) = match setup {
        Ok(v) => v,
        Err(e) => {
            with_core(&core, |c| {
                c.job.finished = Some(format!("Could not start: {e:#}"));
            });
            return Ok(());
        }
    };

    // Remember the folder so the window can offer to open it.
    with_core(&core, |c| c.job.out = Some(base.clone()));

    let mut report = json!({
        "tool": concat!("ibackup ", env!("CARGO_PKG_VERSION"), " (interface)"),
        "started": chrono::Local::now().to_rfc3339(),
        "device": { "udid": udid },
        "stages": {},
    });

    let record = |key: &str, r: &Result<Stats>, report: &mut Json| {
        report["stages"][key] = stage_json(r);
    };

    // Stage 1: device backup
    if opts.backup {
        stage_set(&core, "device-backup", StageStatus::Running, &[]);
        let r: Result<Stats> =
            match backup::run(&d, &base.join("backup"), opts.full, opts.require_enc).await {
                Ok(res) => {
                    let mut s = Stats::default();
                    s.errors = res.errors;
                    s.notes.push(format!(
                        "{} backup at {}",
                        if res.encrypted {
                            "encrypted"
                        } else {
                            "unencrypted"
                        },
                        res.dir.display()
                    ));
                    Ok(s)
                }
                Err(e) => Err(anyhow::anyhow!("{e:#}")),
            };
        record("device-backup", &r, &mut report);
        match &r {
            Ok(s) => stage_set(&core, "device-backup", StageStatus::Done, &stage_summary(s)),
            Err(e) => stage_set(
                &core,
                "device-backup",
                StageStatus::Failed,
                &[format!("{e:#}")],
            ),
        }
    }

    // Stage 2: photos & videos
    if opts.media {
        stage_set(&core, "media", StageStatus::Running, &[]);
        let r = media::run(
            &d,
            &base.join("media"),
            opts.everything,
            &[],
            opts.verify,
            None, // deleting photos from the iPhone stays a CLI feature (typed confirmation)
        )
        .await;
        let r = r.map_err(|e| anyhow::anyhow!("{e:#}"));
        record("media", &r, &mut report);
        match &r {
            Ok(s) => stage_set(&core, "media", StageStatus::Done, &stage_summary(s)),
            Err(e) => stage_set(&core, "media", StageStatus::Failed, &[format!("{e:#}")]),
        }
    }

    // Stage 3: app documents
    if opts.appdocs {
        stage_set(&core, "app-documents", StageStatus::Running, &[]);
        let r = app_docs::run(&d, &base.join("app-documents"), &[])
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"));
        record("app-documents", &r, &mut report);
        match &r {
            Ok(s) => stage_set(&core, "app-documents", StageStatus::Done, &stage_summary(s)),
            Err(e) => stage_set(
                &core,
                "app-documents",
                StageStatus::Failed,
                &[format!("{e:#}")],
            ),
        }
    }

    // Stage 4: iCloud
    if opts.icloud {
        stage_set(&core, "icloud", StageStatus::Running, &[]);
        let ic_opts = icloud::Options {
            extra_sources: vec![],
            no_auto_detect: false,
            download: opts.icloud_download,
            icloudpd_user: opts.icloudpd.clone(),
            rclone_remote: None,
        };
        let r = icloud::run(&base.join("icloud"), &ic_opts)
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"));
        record("icloud", &r, &mut report);
        match &r {
            Ok(s) => stage_set(&core, "icloud", StageStatus::Done, &stage_summary(s)),
            Err(e) => stage_set(&core, "icloud", StageStatus::Failed, &[format!("{e:#}")]),
        }
    }

    let all_ok = report["stages"]
        .as_object()
        .map(|o| {
            o.values()
                .all(|v| v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false))
        })
        .unwrap_or(true);
    report["finished"] = json!(chrono::Local::now().to_rfc3339());
    report["ok"] = json!(all_ok);
    if let Ok(bytes) = serde_json::to_vec_pretty(&report) {
        let _ = std::fs::write(base.join("report.json"), bytes);
    }

    let msg = if all_ok {
        format!("Everything is in {}", base.display())
    } else {
        format!(
            "Some stages reported errors. Details: {}",
            base.join("report.json").display()
        )
    };
    with_core(&core, |c| {
        c.job.ok = all_ok;
        c.job.finished = Some(msg.clone());
    });
    util::say(&msg);
    Ok(())
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

struct App {
    core: Shared,
    tx: Sender<Task>,

    // Window bits that are not engine state.
    icon: Option<egui::TextureHandle>,
    show_log: bool,
    enc_open: bool,
    confirm_quit: bool,
    /// Progress dialog — opened when a job starts, hideable while it runs.
    show_progress: bool,
    /// How many times we asked the window manager to fill the screen.
    maximize_tries: u8,

    // Cached view of the shared state (small; cloned each frame).
    busy: Option<String>,
    devices: Vec<DeviceEntry>,
    devices_error: Option<String>,
    job: Job,
    log: Vec<String>,
    gauges: Vec<Gauge>,

    // Free-space probe, refreshed lazily instead of every frame.
    dest_free: Option<u64>,
    dest_free_for: String,
    dest_free_at: Option<Instant>,

    // Form fields
    sel: usize,
    out: String,
    do_backup: bool,
    full: bool,
    require_enc: bool,
    do_media: bool,
    everything: bool,
    verify: bool,
    do_appdocs: bool,
    do_icloud: bool,
    icloud_download: bool,
    icloudpd: String,
    password: String,
    folder_rx: Option<Receiver<Option<PathBuf>>>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync();
        self.handle_dropped_folder(ctx);
        self.fill_screen(ctx);


        // Closing while a job runs would stop it; ask first (it would resume
        // later, but silently losing a long copy is not a nice surprise).
        if ctx.input(|i| i.viewport().close_requested()) && self.busy.is_some() && !self.confirm_quit
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }

        self.header(ctx);
        if self.show_log {
            self.activity_panel(ctx);
        }
        self.plan_panel(ctx);
        self.main_panel(ctx);
        self.progress_modal(ctx);
        self.quit_dialog(ctx);

        let running = self.job.started.is_some() && self.job.finished.is_none();
        if self.busy.is_some() || running {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, tx: Sender<Task>, core: Shared) -> Self {
        t::install(&cc.egui_ctx);
        // Let the worker wake the *real* window, not a throwaway context.
        with_core(&core, |c| c.ctx = cc.egui_ctx.clone());

        let app = App {
            core,
            tx: tx.clone(),
            icon: load_icon(&cc.egui_ctx),
            show_log: true,
            enc_open: false,
            confirm_quit: false,
            show_progress: false,
            maximize_tries: 0,
            busy: None,
            devices: Vec::new(),
            devices_error: None,
            job: Job::default(),
            log: Vec::new(),
            gauges: Vec::new(),
            dest_free: None,
            dest_free_for: String::new(),
            dest_free_at: None,
            sel: 0,
            out: default_out_dir(),
            do_backup: true,
            full: false,
            require_enc: false,
            do_media: true,
            everything: false,
            verify: true,
            do_appdocs: true,
            do_icloud: true,
            icloud_download: false,
            icloudpd: String::new(),
            password: String::new(),
            folder_rx: None,
        };
        tx.send(Task::Refresh).ok();
        app
    }

    /// Fill the screen on the first few frames.
    ///
    /// The polite way is to ask the window manager to maximise us, which is what
    /// happens on a normal desktop. Some sessions never act on that request
    /// though (rootless Xwayland reports no monitor at all, and then even GTK
    /// apps stay at their requested size), so after a short grace period we size
    /// the window to the desktop work area ourselves.
    fn fill_screen(&mut self, ctx: &egui::Context) {
        if self.maximize_tries >= 4 {
            return;
        }
        let viewport = ctx.input(|i| i.viewport().clone());
        let inner = viewport.inner_rect.map(|r| r.size()).unwrap_or_default();
        let Some(area) = work_area(ctx) else {
            self.maximize_tries = 4;
            return;
        };

        match self.maximize_tries {
            0 => {
                self.maximize_tries = 1;
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                ctx.request_repaint_after(Duration::from_millis(700));
            }
            1 => {
                // The window manager did the work: we are filling the screen.
                if inner.x >= area.width() * 0.9 && inner.y >= area.height() * 0.8 {
                    self.maximize_tries = 4;
                    return;
                }
                // It did not, so take the screen ourselves. A maximised window
                // ignores resize requests, so leave that state first and let the
                // window manager settle before resizing.
                self.maximize_tries = 2;
                if viewport.maximized == Some(true) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
                }
                ctx.request_repaint_after(Duration::from_millis(400));
            }
            2 => {
                self.maximize_tries = 3;
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(area.min));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(area.size()));
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            _ => {
                // Some window managers place the window themselves and ignore
                // the requested position, so finish by measuring where we
                // actually landed and sizing the window to the rest of the
                // desktop. The desktop area also excludes the title bar and any
                // panel, which keeps both of them on screen.
                self.maximize_tries = 4;
                let chrome = match (viewport.outer_rect, viewport.inner_rect) {
                    (Some(outer), Some(inner)) => (outer.height() - inner.height()).max(0.0),
                    _ => 0.0,
                };
                let origin = viewport.inner_rect.map(|r| r.min).unwrap_or(area.min);
                let size = egui::vec2(
                    (area.max.x - origin.x).max(480.0),
                    (area.max.y - origin.y - chrome).max(360.0),
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
        }
    }

    /// Pull the latest state from the worker thread each frame.
    fn sync(&mut self) {
        if let Some(rx) = &self.folder_rx {
            match rx.try_recv() {
                Ok(picked) => {
                    self.folder_rx = None;
                    if let Some(p) = picked {
                        self.out = p.display().to_string();
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.folder_rx = None,
            }
        }

        {
            let core = self.core.lock().unwrap_or_else(|p| p.into_inner());
            self.busy = core.busy.clone();
            self.job = core.job.clone();
            self.devices = core.devices.clone();
            self.devices_error = core.devices_error.clone();
        }
        self.log.extend(util::ui_take_log());
        if self.log.len() > 500 {
            self.log.drain(0..self.log.len() - 500);
        }
        self.gauges = util::ui_gauges();
        if self.sel >= self.devices.len() {
            self.sel = self.devices.len().saturating_sub(1);
        }
        self.refresh_free_space();
    }

    /// Re-probe the destination now and then. Deliberately does not schedule a
    /// repaint: while a job runs the frame loop already ticks every 200 ms, and
    /// when idle there is nothing worth waking the window for.
    fn refresh_free_space(&mut self) {
        let stale = self
            .dest_free_at
            .map(|t| t.elapsed() >= Duration::from_secs(4))
            .unwrap_or(true);
        if stale || self.dest_free_for != self.out {
            self.dest_free = util::free_space(&expand_home(&self.out));
            self.dest_free_for = self.out.clone();
            self.dest_free_at = Some(Instant::now());
        }
    }

    fn handle_dropped_folder(&mut self, ctx: &egui::Context) {
        let dropped: Option<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|f| f.path.clone())
        });
        if let Some(p) = dropped {
            let dir = if p.is_dir() {
                p
            } else {
                p.parent().map(Path::to_path_buf).unwrap_or(p)
            };
            self.out = dir.display().to_string();
            self.dest_free_at = None;
        }
    }

    fn busy(&self) -> bool {
        self.busy.is_some()
    }

    fn selected(&self) -> Option<&DeviceEntry> {
        self.devices.get(self.sel)
    }

    fn status(&self) -> (String, Color32) {
        if let Some(label) = &self.busy {
            return (label.clone(), t::ACCENT);
        }
        if self.job.finished.is_some() {
            return if self.job.ok {
                ("Backup complete".to_string(), t::SUCCESS)
            } else {
                ("Finished with issues".to_string(), t::WARN)
            };
        }
        if self.devices_error.is_some() {
            ("Connection problem".to_string(), t::DANGER)
        } else if self.devices.is_empty() {
            ("No iPhone".to_string(), t::TEXT_DIM)
        } else {
            ("Ready".to_string(), t::SUCCESS)
        }
    }

    // -- panels -------------------------------------------------------------

    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::none()
                    .fill(t::PANEL)
                    .inner_margin(egui::Margin::symmetric(18.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(icon) = &self.icon {
                        ui.add(
                            egui::Image::new(egui::load::SizedTexture::new(
                                icon.id(),
                                Vec2::splat(30.0),
                            ))
                            .rounding(Rounding::same(8.0)),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(RichText::new("ibackup").size(17.0).strong().color(t::TEXT));
                        ui.label(
                            RichText::new("Back up your iPhone to this computer")
                                .size(11.0)
                                .color(t::TEXT_DIM),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (label, color) = self.status();
                        t::pill(ui, &label, color);
                        if self.busy.is_some() {
                            ui.add(egui::Spinner::new().size(13.0).color(color));
                        }
                        let version = ui.label(
                            RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                                .size(11.0)
                                .color(t::TEXT_DIM),
                        );
                        version.on_hover_text("ibackup — iPhone backup tool");
                        if t::ghost_button(
                            ui,
                            if self.show_log { "Hide activity" } else { "Activity" },
                        )
                        .clicked()
                        {
                            self.show_log = !self.show_log;
                        }
                    });
                });
            });
    }

    fn activity_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("activity")
            .resizable(true)
            .default_height(140.0)
            .min_height(64.0)
            .frame(
                egui::Frame::none()
                    .fill(t::PANEL)
                    .inner_margin(egui::Margin::symmetric(18.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.label(RichText::new("ACTIVITY").size(10.0).color(t::TEXT_DIM));
                ui.add_space(2.0);
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(
                                RichText::new("Engine messages appear here.")
                                    .size(11.0)
                                    .color(t::TEXT_DIM),
                            );
                        }
                        for line in &self.log {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(line).monospace().size(11.0).color(t::TEXT_DIM),
                                )
                                .wrap(),
                            );
                        }
                    });
            });
    }

    fn plan_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("plan")
            .exact_width(318.0)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(t::PANEL)
                    .inner_margin(egui::Margin::symmetric(16.0, 16.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Backup plan").size(16.0).strong().color(t::TEXT));
                        ui.add_space(1.0);
                        ui.label(
                            RichText::new("Review, then start.").size(11.5).color(t::TEXT_DIM),
                        );
                        ui.add_space(12.0);

                        t::card(ui, |ui| self.plan_details(ui));
                        ui.add_space(14.0);

                        let can_start = self.can_start();
                        let label = if self.busy() { "Working…" } else { "Start backup" };
                        let response = t::primary_button(ui, label, can_start);
                        let clicked = response.clicked();
                        if !can_start && !self.busy() {
                            response.on_disabled_hover_text(self.why_not());
                        }
                        if clicked {
                            self.start();
                        }
                        ui.add_space(8.0);

                        if self.busy() {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        "Closing the window stops the job. Running it again resumes where it stopped.",
                                    )
                                    .size(11.0)
                                    .color(t::TEXT_DIM),
                                )
                                .wrap(),
                            );
                            ui.add_space(2.0);
                            if t::secondary_button(ui, "Show backup progress", true).clicked() {
                                self.show_progress = true;
                            }
                        } else {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        "Nothing is uploaded. Everything stays in the folder you choose.",
                                    )
                                    .size(11.0)
                                    .color(t::TEXT_DIM),
                                )
                                .wrap(),
                            );
                            if let Some(out) = self.job.out.clone().filter(|_| self.job.finished.is_some()) {
                                ui.add_space(8.0);
                                if t::secondary_button(ui, "Open backup folder", true).clicked() {
                                    reveal(&out);
                                }
                            }
                        }
                    });
            });
    }

    fn plan_details(&self, ui: &mut egui::Ui) {
        match self.selected() {
            Some(d) => {
                let name = if d.name.is_empty() {
                    short_udid(&d.udid)
                } else {
                    d.name.clone()
                };
                t::field(ui, "IPHONE", &name, t::TEXT);
                let system = if d.paired {
                    format!("{} · iOS {}", d.model, d.ios)
                } else {
                    "Not paired yet".to_string()
                };
                t::field(ui, "SYSTEM", &system, t::TEXT);
            }
            None => t::field(ui, "IPHONE", "Not connected", t::TEXT_DIM),
        }

        let (dest, dest_color) = if self.out.trim().is_empty() {
            ("Not chosen".to_string(), t::WARN)
        } else {
            (self.out.trim().to_string(), t::TEXT)
        };
        t::field(ui, "DESTINATION", &dest, dest_color);

        if let Some(free) = self.dest_free {
            let color = if free < 5 * GB { t::WARN } else { t::TEXT };
            t::field(ui, "FREE SPACE", &format!("{} free", human_bytes(free)), color);
        }

        t::divider(ui);
        ui.label(RichText::new("INCLUDES").size(10.0).color(t::TEXT_DIM));
        ui.add_space(4.0);
        let mut chips: Vec<(String, Color32)> = Vec::new();
        if self.do_backup {
            chips.push(("Device backup".to_string(), t::ACCENT));
        }
        if self.do_media {
            chips.push(("Photos & videos".to_string(), t::ACCENT));
        }
        if self.do_appdocs {
            chips.push(("App documents".to_string(), t::ACCENT));
        }
        if self.do_icloud {
            chips.push(("iCloud".to_string(), t::ACCENT));
        }
        if chips.is_empty() {
            chips.push(("Nothing selected".to_string(), t::WARN));
        }
        t::chips(ui, &chips);

        let mut extra: Vec<&str> = Vec::new();
        if self.do_backup && self.full {
            extra.push("full, not incremental");
        }
        if self.do_backup && self.require_enc {
            extra.push("requires encryption");
        }
        if self.do_media && self.everything {
            extra.push("whole media partition");
        }
        if !extra.is_empty() {
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(extra.join(" · ")).size(11.0).color(t::TEXT_DIM),
                )
                .wrap(),
            );
        }
    }

    fn main_panel(&mut self, ctx: &egui::Context) {
        // On a wide screen the cards would stretch into unreadable strips, so the
        // column keeps a comfortable measure and the panel grows side gutters
        // instead. 318 is the plan panel; the same constant lives in plan_panel.
        let gutter = ((ctx.screen_rect().width() - 318.0 - 980.0) / 2.0).max(20.0);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(t::BG).inner_margin(egui::Margin {
                left: gutter.floor(),
                right: gutter.floor(),
                top: 16.0,
                bottom: 16.0,
            }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Once a job exists its progress leads: it is the thing the
                        // user came back to look at.
                        let has_job = !self.job.stages.is_empty()
                            || !self.gauges.is_empty()
                            || self.job.finished.is_some();
                        if has_job {
                            self.progress_card(ui);
                            ui.add_space(12.0);
                        }
                        self.device_card(ui);
                        ui.add_space(12.0);
                        self.destination_card(ui);
                        ui.add_space(12.0);
                        self.options_card(ui);
                        ui.add_space(4.0);
                    });
            });
    }

    fn device_card(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        let dev = self.selected().cloned();
        t::card(ui, |ui| {
            ui.horizontal(|ui| {
                t::card_heading(ui, "iPhone", "Connected over USB");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if t::secondary_button(ui, "Refresh", !busy).clicked() {
                        self.tx.send(Task::Refresh).ok();
                    }
                });
            });
            ui.add_space(10.0);

            if let Some(err) = self.devices_error.clone() {
                t::banner(ui, t::DANGER, "Could not reach the iPhone", &err, |ui| {
                    ui.add_space(6.0);
                    if t::secondary_button(ui, "Try again", !busy).clicked() {
                        self.tx.send(Task::Refresh).ok();
                    }
                });
                return;
            }

            let Some(d) = dev else {
                ui.label(
                    RichText::new("No iPhone detected")
                        .size(14.5)
                        .strong()
                        .color(t::TEXT),
                );
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Connect your iPhone with a USB cable, unlock it, then tap “Trust This Computer”.",
                        )
                        .size(12.0)
                        .color(t::TEXT_DIM),
                    )
                    .wrap(),
                );
                ui.add_space(10.0);
                if t::accent_button(ui, "Search again", !busy).clicked() {
                    self.tx.send(Task::Refresh).ok();
                }
                return;
            };

            if self.devices.len() > 1 {
                let devices = self.devices.clone();
                let selected_text = device_label(&d);
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("device")
                        .selected_text(selected_text)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for (i, entry) in devices.iter().enumerate() {
                                ui.selectable_value(&mut self.sel, i, device_label(entry));
                            }
                        });
                });
                ui.add_space(10.0);
            }

            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let name = if d.name.is_empty() {
                        short_udid(&d.udid)
                    } else {
                        d.name.clone()
                    };
                    ui.label(RichText::new(name).size(17.0).strong().color(t::TEXT));
                    let sub = if d.paired {
                        format!("{} · iOS {}", d.model, d.ios)
                    } else {
                        "Waiting for this computer to be trusted".to_string()
                    };
                    ui.label(RichText::new(sub).size(12.0).color(t::TEXT_DIM));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if d.paired {
                        match d.encrypted {
                            Some(true) => t::pill(ui, "Encrypted", t::SUCCESS),
                            Some(false) => t::pill(ui, "Unencrypted", t::WARN),
                            None => t::pill(ui, "Unknown", t::TEXT_DIM),
                        }
                    }
                    if d.paired {
                        t::pill(ui, "Paired", t::SUCCESS);
                    } else {
                        t::pill(ui, "Not paired", t::WARN);
                    }
                });
            });

            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("UDID {}", short_udid(&d.udid)))
                    .monospace()
                    .size(10.5)
                    .color(t::TEXT_DIM),
            );

            if !d.paired {
                ui.add_space(10.0);
                let udid = d.udid.clone();
                t::banner(
                    ui,
                    t::ACCENT,
                    "One step: trust this computer",
                    "Unlock the iPhone, tap “Trust” on its screen, then enter the passcode. Photos need the phone unlocked and connected.",
                    |ui| {
                        ui.add_space(6.0);
                        if t::accent_button(ui, "Pair now", !busy).clicked() {
                            self.tx.send(Task::Pair(udid)).ok();
                        }
                    },
                );
                return;
            }

            // Paired: encryption controls, tucked behind a toggle.
            t::divider(ui);
            let on = d.encrypted.unwrap_or(false);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Backup encryption")
                        .size(12.5)
                        .strong()
                        .color(t::TEXT),
                );
                if on {
                    t::pill(ui, "ON", t::SUCCESS);
                } else {
                    t::pill(ui, "OFF", t::WARN);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if t::ghost_button(ui, if self.enc_open { "Hide" } else { "Settings" })
                        .clicked()
                    {
                        self.enc_open = !self.enc_open;
                    }
                });
            });
            ui.add(
                egui::Label::new(
                    RichText::new(if on {
                        "Health, Keychain passwords and Wi-Fi settings are included in the backup."
                    } else {
                        "Health, Keychain passwords and Wi-Fi settings are left out. Turn it on for a complete backup."
                    })
                    .size(11.5)
                    .color(t::TEXT_DIM),
                )
                .wrap(),
            );

            if self.enc_open {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        !busy,
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .hint_text("backup password")
                            .desired_width(200.0),
                    );
                    let label = if on { "Turn OFF" } else { "Turn ON" };
                    let enabled = !self.password.is_empty() && !busy;
                    if t::accent_button(ui, label, enabled).clicked() {
                        self.tx
                            .send(Task::Enc {
                                udid: d.udid.clone(),
                                enable: !on,
                                password: self.password.clone(),
                            })
                            .ok();
                        self.password.clear();
                    }
                });
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "The iPhone asks for its passcode to confirm. Keep the password safe — an encrypted backup cannot be read without it.",
                        )
                        .size(11.0)
                        .color(t::TEXT_DIM),
                    )
                    .wrap(),
                );
            }
        });
    }

    fn destination_card(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        t::card(ui, |ui| {
            t::card_heading(ui, "Destination", "Where the backup folders are written");
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                let field_width = (ui.available_width() - 100.0).max(120.0);
                ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut self.out)
                        .hint_text("~/iPhoneBackups")
                        .desired_width(field_width),
                );
                if t::secondary_button(ui, "Browse…", !busy).clicked() {
                    let (tx, rx) = channel();
                    self.folder_rx = Some(rx);
                    std::thread::spawn(move || {
                        let picked = rfd::FileDialog::new()
                            .set_title("Where should backups go?")
                            .pick_folder();
                        let _ = tx.send(picked);
                    });
                }
            });

            ui.add_space(8.0);
            match self.dest_free {
                Some(free) => {
                    let low = free < 5 * GB;
                    ui.horizontal(|ui| {
                        t::pill(
                            ui,
                            &format!("{} free", human_bytes(free)),
                            if low { t::WARN } else { t::SUCCESS },
                        );
                        if low {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        "Low on room — a full backup can need many gigabytes.",
                                    )
                                    .size(11.0)
                                    .color(t::WARN),
                                )
                                .wrap(),
                            );
                        }
                    });
                }
                None => {
                    ui.label(
                        RichText::new("Free space unknown — the folder may not exist yet.")
                            .size(11.0)
                            .color(t::TEXT_DIM),
                    );
                }
            }

            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Each iPhone gets its own subfolder here. Tip: drag a folder onto the window to use it.",
                    )
                    .size(11.0)
                    .color(t::TEXT_DIM),
                )
                .wrap(),
            );
        });
    }

    fn options_card(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        let enabled = !busy;
        t::card(ui, |ui| {
            t::card_heading(ui, "What to copy", "Stages run top to bottom");
            ui.add_space(10.0);

            t::option_row(
                ui,
                &mut self.do_backup,
                "Full device backup",
                "Restorable Finder/iTunes-style backup: settings, messages, Health and app data.",
                enabled,
            );
            if self.do_backup {
                t::sub_checkbox(ui, &mut self.full, "Force a full copy instead of an incremental one", enabled);
                t::sub_checkbox(ui, &mut self.require_enc, "Refuse to run unless encryption is ON", enabled);
                ui.add_space(4.0);
            }

            t::option_row(
                ui,
                &mut self.do_media,
                "Photos & videos",
                "Plain files you can open anywhere; camera roll or the whole media partition.",
                enabled,
            );
            if self.do_media {
                t::sub_checkbox(
                    ui,
                    &mut self.verify,
                    "Verify copies against hashes the iPhone computes (recommended)",
                    enabled,
                );
                t::sub_checkbox(
                    ui,
                    &mut self.everything,
                    "Whole media partition, not just the camera roll",
                    enabled,
                );
                ui.add_space(4.0);
            }

            t::option_row(
                ui,
                &mut self.do_appdocs,
                "App documents",
                "Each app's shared Documents folder, copied as plain files.",
                enabled,
            );

            t::option_row(
                ui,
                &mut self.do_icloud,
                "iCloud folders",
                "iCloud Drive and photos already synced to this computer.",
                enabled,
            );
            if self.do_icloud {
                t::sub_checkbox(
                    ui,
                    &mut self.icloud_download,
                    "macOS: download cloud-only files first",
                    enabled && cfg!(target_os = "macos"),
                );
                ui.horizontal(|ui| {
                    ui.add_space(29.0);
                    ui.add_enabled(
                        enabled,
                        egui::TextEdit::singleline(&mut self.icloudpd)
                            .hint_text("Apple ID for iCloud Photos (optional)")
                            .desired_width(260.0),
                    );
                });
            }

            ui.add_space(10.0);
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Deleting verified photos from the iPhone needs a typed confirmation, so it stays a CLI feature: ibackup media --out … --delete-verified",
                    )
                    .size(11.0)
                    .color(t::TEXT_DIM),
                )
                .wrap(),
            );
        });
    }

    fn progress_card(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        t::card(ui, |ui| {
            ui.horizontal(|ui| {
                t::card_heading(ui, "Progress", "");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(started) = self.job.started {
                        let text = if busy || self.job.finished.is_none() {
                            format!("{} elapsed", fmt_duration(started.elapsed()))
                        } else {
                            format!("took {}", fmt_duration(started.elapsed()))
                        };
                        t::pill(ui, &text, t::TEXT_DIM);
                    }
                });
            });

            if let Some(label) = self.busy.clone() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0).color(t::ACCENT));
                    ui.label(RichText::new(label).size(12.0).color(t::ACCENT));
                });
            }

            if !self.job.stages.is_empty() {
                ui.add_space(8.0);
                for stage in &self.job.stages {
                    stage_row(ui, stage);
                    for line in &stage.lines {
                        stage_line(ui, line);
                    }
                }
            }

            if !self.gauges.is_empty() {
                ui.add_space(8.0);
                for g in &self.gauges {
                    gauge_row(ui, g);
                }
            }

            if !busy {
                if let Some(msg) = self.job.finished.clone() {
                    ui.add_space(10.0);
                    let (color, title) = if self.job.ok {
                        (t::SUCCESS, "Backup complete")
                    } else {
                        (t::WARN, "Finished with issues")
                    };
                    let out = self.job.out.clone();
                    t::banner(ui, color, title, &msg, |ui| {
                        if let Some(out) = out {
                            ui.add_space(8.0);
                            if t::secondary_button(ui, "Open backup folder", true).clicked() {
                                reveal(&out);
                            }
                        }
                    });
                }
            }
        });
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        // Nothing left to stop: dismiss a dialog the user left open.
        if !self.confirm_quit || !self.busy() {
            self.confirm_quit = false;
            return;
        }
        egui::Window::new("Stop the backup?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "A backup is still running. Closing now stops it; running ibackup again later resumes every stage where it stopped.",
                        )
                        .size(12.0)
                        .color(t::TEXT_DIM),
                    )
                    .wrap(),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if t::accent_button(ui, "Keep backing up", true).clicked() {
                        self.confirm_quit = false;
                    }
                    if t::secondary_button(ui, "Stop and quit", true).clicked() {
                        self.confirm_quit = false;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
    }

    // -- actions ------------------------------------------------------------

    fn can_start(&self) -> bool {
        !self.busy()
            && self.selected().is_some()
            && !self.out.trim().is_empty()
            && (self.do_backup || self.do_media || self.do_appdocs || self.do_icloud)
    }

    fn why_not(&self) -> &'static str {
        if self.busy() {
            "A task is already running."
        } else if self.selected().is_none() {
            "Connect an iPhone over USB first."
        } else if self.out.trim().is_empty() {
            "Choose a destination folder."
        } else {
            "Select at least one thing to copy."
        }
    }

    fn start(&mut self) {
        let Some(d) = self.selected().cloned() else { return };
        let options = Options {
            out: expand_home(&self.out),
            backup: self.do_backup,
            full: self.full,
            require_enc: self.require_enc,
            media: self.do_media,
            everything: self.everything,
            verify: self.verify,
            appdocs: self.do_appdocs,
            icloud: self.do_icloud,
            icloud_download: self.icloud_download,
            icloudpd: (!self.icloudpd.trim().is_empty()).then(|| self.icloudpd.trim().to_string()),
        };
        self.tx.send(Task::Run { udid: d.udid, options }).ok();
        self.show_progress = true;
    }

    // -- progress dialog ----------------------------------------------------

    /// The copy runs behind a dimmed overlay so it is visible from wherever the
    /// user is looking, instead of below three cards they have to scroll past.
    fn progress_modal(&mut self, ctx: &egui::Context) {
        let busy = self.busy();
        let running = busy || self.job.started.is_some();
        if !self.show_progress || !running {
            self.show_progress = false;
            return;
        }

        let screen = ctx.screen_rect();
        let dismissed = ctx.input(|i| i.key_pressed(egui::Key::Escape));

        // Veil: dims the window, swallows stray clicks, hides on a click outside.
        let veil = egui::Area::new(egui::Id::new("progress-veil"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, response) = ui.allocate_exact_size(screen.size(), Sense::click());
                ui.painter().rect_filled(
                    rect,
                    Rounding::ZERO,
                    Color32::from_rgba_unmultiplied(4, 7, 11, 200),
                );
                response
            })
            .inner;

        if veil.clicked() || dismissed {
            self.show_progress = false;
            return;
        }

        let width = 520.0_f32.min(screen.width() - 64.0);
        egui::Area::new(egui::Id::new("progress-dialog"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::new(0.0, -6.0))
            .show(ctx, |ui| {
                ui.set_width(width);
                egui::Frame::none()
                    .fill(t::SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, t::BORDER_HI))
                    .rounding(Rounding::same(16.0))
                    .inner_margin(egui::Margin::same(20.0))
                    .show(ui, |ui| {
                        ui.set_width(width - 40.0);
                        self.progress_body(ui, busy);
                    });
            });
    }

    fn progress_body(&mut self, ui: &mut egui::Ui, busy: bool) {
        let (title, color) = if busy {
            ("Backing up", t::TEXT)
        } else if self.job.ok {
            ("Backup complete", t::SUCCESS)
        } else {
            ("Backup finished with issues", t::WARN)
        };

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.horizontal(|ui| {
                    if busy {
                        ui.add(egui::Spinner::new().size(15.0).color(t::ACCENT));
                    }
                    ui.label(RichText::new(title).size(17.0).strong().color(color));
                });
                let mut parts: Vec<String> = Vec::new();
                if let Some(d) = self.selected() {
                    parts.push(if d.name.is_empty() {
                        short_udid(&d.udid)
                    } else {
                        d.name.clone()
                    });
                }
                if let Some(out) = self.job.out.clone() {
                    parts.push(out.display().to_string());
                }
                if !parts.is_empty() {
                    ui.add(
                        egui::Label::new(
                            RichText::new(parts.join("   →   ")).size(11.5).color(t::TEXT_DIM),
                        )
                        .truncate(),
                    );
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if let Some(started) = self.job.started {
                    let text = if busy {
                        format!("{} elapsed", fmt_duration(started.elapsed()))
                    } else {
                        format!("took {}", fmt_duration(started.elapsed()))
                    };
                    t::pill(ui, &text, if busy { t::ACCENT } else { t::TEXT_DIM });
                }
            });
        });

        ui.add_space(12.0);
        t::divider(ui);
        ui.add_space(8.0);

        egui::ScrollArea::vertical()
            .max_height((ui.ctx().screen_rect().height() * 0.42).max(160.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if !self.gauges.is_empty() {
                    for g in &self.gauges {
                        gauge_row(ui, g);
                    }
                    ui.add_space(6.0);
                }
                for stage in &self.job.stages {
                    stage_row(ui, stage);
                    for line in &stage.lines {
                        stage_line(ui, line);
                    }
                }
                if self.job.stages.is_empty() {
                    let label = self.busy.clone().unwrap_or_else(|| "Starting…".to_string());
                    ui.add(
                        egui::Label::new(RichText::new(label).size(12.0).color(t::TEXT_DIM)).wrap(),
                    );
                }
            });

        if let Some(msg) = self.job.finished.clone().filter(|_| !busy) {
            ui.add_space(10.0);
            ui.add(egui::Label::new(RichText::new(msg).size(11.5).color(t::TEXT_DIM)).wrap());
        }

        ui.add_space(14.0);
        let mut hide = false;
        if busy {
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new("Keeps running if you hide this.")
                            .size(11.0)
                            .color(t::TEXT_DIM),
                    )
                    .wrap(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if t::secondary_button(ui, "Hide", true).clicked() {
                        hide = true;
                    }
                });
            });
        } else {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if t::accent_button(ui, "Done", true).clicked() {
                    hide = true;
                }
                if let Some(out) = self.job.out.clone() {
                    if t::secondary_button(ui, "Open folder", true).clicked() {
                        reveal(&out);
                    }
                }
            });
        }
        if hide {
            self.show_progress = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Small drawing helpers
// ---------------------------------------------------------------------------

fn stage_row(ui: &mut egui::Ui, stage: &Stage) {
    let (word, color) = match stage.status {
        StageStatus::Pending => ("Queued", t::TEXT_DIM),
        StageStatus::Running => ("Running", t::ACCENT),
        StageStatus::Done => ("Done", t::SUCCESS),
        StageStatus::Failed => ("Failed", t::DANGER),
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        let title_color = if stage.status == StageStatus::Pending {
            t::TEXT_DIM
        } else {
            t::TEXT
        };
        ui.label(RichText::new(stage.title).size(13.0).color(title_color));
        if stage.status == StageStatus::Running {
            ui.add(egui::Spinner::new().size(12.0).color(color));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            t::pill(ui, word, color);
        });
    });
}

fn stage_line(ui: &mut egui::Ui, line: &str) {
    let (text, color) = if let Some(rest) = line.strip_prefix("error: ") {
        (rest, t::DANGER)
    } else if let Some(rest) = line.strip_prefix("note: ") {
        (rest, t::TEXT_DIM)
    } else {
        (line, t::TEXT_DIM)
    };
    ui.horizontal(|ui| {
        ui.add_space(18.0);
        ui.add(
            egui::Label::new(RichText::new(text).size(11.5).color(color)).wrap(),
        );
    });
}

fn gauge_row(ui: &mut egui::Ui, g: &Gauge) {
    let frac = g
        .total
        .filter(|total| *total > 0)
        .map(|total| (g.position as f32 / total as f32).clamp(0.0, 1.0));

    let Some(frac) = frac else {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(11.0).color(t::ACCENT));
            let text = if g.unit == GaugeUnit::Bytes {
                format!("{}: {} copied", g.label, human_bytes(g.position))
            } else if g.message.is_empty() {
                g.label.clone()
            } else {
                format!("{} {}", g.label, g.message)
            };
            ui.add(egui::Label::new(RichText::new(text).size(11.5).color(t::TEXT_DIM)).wrap());
        });
        return;
    };

    let (left, right) = match g.unit {
        GaugeUnit::Percent => (
            if g.message.is_empty() {
                g.label.clone()
            } else {
                g.message.clone()
            },
            format!("{:.0}%", frac * 100.0),
        ),
        _ => (
            if g.label.is_empty() {
                g.message.clone()
            } else {
                g.label.clone()
            },
            format!(
                "{} / {}",
                human_bytes(g.position),
                human_bytes(g.total.unwrap_or(0))
            ),
        ),
    };

    ui.horizontal(|ui| {
        // Truncate rather than wrap: the byte counts on the right stay readable.
        ui.add(egui::Label::new(RichText::new(left).size(11.5).color(t::TEXT)).truncate());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(right).size(11.0).color(t::TEXT_DIM));
        });
    });
    ui.add(
        egui::ProgressBar::new(frac)
            .desired_height(7.0)
            .rounding(Rounding::same(4.0))
            .fill(t::ACCENT)
            .animate(true),
    );
    ui.add_space(6.0);
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn device_label(d: &DeviceEntry) -> String {
    if d.paired {
        format!("{} (iOS {})", d.name, d.ios)
    } else {
        format!("{} — not paired yet", short_udid(&d.udid))
    }
}

/// Show a folder in the platform file manager.
fn reveal(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(path).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    let _ = path;
}

fn expand_home(p: &str) -> PathBuf {
    let trimmed = p.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = trimmed.strip_prefix("~/").or(trimmed.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

/// Desktop area in egui points that the window should fill, if we can work it
/// out.
///
/// Compositors advertise the monitor size through the viewport, except on
/// rootless Xwayland sessions, where that comes back as 1x1 and no window can be
/// maximised at all; there we read the window manager's `_NET_WORKAREA` from the
/// X root window instead.
fn work_area(ctx: &egui::Context) -> Option<egui::Rect> {
    let ppp = ctx.pixels_per_point();
    let ppp = if ppp > 0.1 { ppp } else { 1.0 };
    if let Some((x, y, width, height)) = x11_work_area() {
        return Some(egui::Rect::from_min_size(
            egui::pos2(x / ppp, y / ppp),
            egui::vec2(width / ppp, height / ppp),
        ));
    }
    let reported = ctx.input(|i| i.viewport().monitor_size)?;
    if reported.x >= 320.0 && reported.y >= 240.0 {
        return Some(egui::Rect::from_min_size(egui::Pos2::ZERO, reported));
    }
    None
}

/// `_NET_WORKAREA` of the first desktop in pixels, read straight from an X
/// server when one is reachable.
///
/// `libX11` is loaded at runtime so that builds without the X11 backend keep
/// working; every failure path simply returns `None`.
///
/// Only Linux has an X server to ask, so the whole Xlib block below is compiled
/// out elsewhere: `dlopen`/`dlsym` do not exist on Windows, and referring to
/// them there fails the link (other hosts fall back to the viewport size).
#[cfg(target_os = "linux")]
fn x11_work_area() -> Option<(f32, f32, f32, f32)> {
    use std::ffi::{c_char, c_int, c_long, c_ulong, c_void, CStr};

    type Display = c_void;
    type OpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut Display;
    type CloseDisplay = unsafe extern "C" fn(*mut Display) -> c_int;
    type RootWindow = unsafe extern "C" fn(*mut Display) -> c_ulong;
    type InternAtom = unsafe extern "C" fn(*mut Display, *const c_char, c_int) -> c_ulong;
    type GetProperty = unsafe extern "C" fn(
        *mut Display,
        c_ulong,
        c_ulong,
        c_long,
        c_long,
        c_int,
        c_ulong,
        *mut c_ulong,
        *mut c_int,
        *mut c_ulong,
        *mut c_ulong,
        *mut *mut u8,
    ) -> c_int;
    type FreeData = unsafe extern "C" fn(*mut c_void) -> c_int;

    extern "C" {
        fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    /// `RTLD_NOW`.
    const RTLD_NOW: c_int = 2;
    /// `XA_CARDINAL`, the property type `_NET_WORKAREA` is published as.
    const XA_CARDINAL: c_ulong = 6;

    // SAFETY: every pointer below comes from libX11 and is used with the
    // documented Xlib signature, mirrored by the types above. All pointers are
    // null-checked before use, and the returned property data is freed once.
    unsafe {
        let lib = dlopen(c"libX11.so.6".as_ptr(), RTLD_NOW);
        if lib.is_null() {
            return None;
        }
        let symbol = |name: &CStr| {
            let ptr = dlsym(lib, name.as_ptr());
            if ptr.is_null() { None } else { Some(ptr) }
        };
        let open: OpenDisplay = std::mem::transmute(symbol(c"XOpenDisplay")?);
        let close: CloseDisplay = std::mem::transmute(symbol(c"XCloseDisplay")?);
        let root_window: RootWindow = std::mem::transmute(symbol(c"XDefaultRootWindow")?);
        let intern_atom: InternAtom = std::mem::transmute(symbol(c"XInternAtom")?);
        let get_property: GetProperty = std::mem::transmute(symbol(c"XGetWindowProperty")?);
        let free: FreeData = std::mem::transmute(symbol(c"XFree")?);

        let display = open(std::ptr::null());
        if display.is_null() {
            return None;
        }

        let mut area = None;
        let atom = intern_atom(display, c"_NET_WORKAREA".as_ptr(), 1);
        if atom != 0 {
            let (mut kind, mut format) = (0, 0);
            let (mut items, mut remaining) = (0, 0);
            let mut data: *mut u8 = std::ptr::null_mut();
            let status = get_property(
                display,
                root_window(display),
                atom,
                0,
                16,
                0,
                XA_CARDINAL,
                &mut kind,
                &mut format,
                &mut items,
                &mut remaining,
                &mut data,
            );
            if status == 0 && !data.is_null() && format == 32 && items >= 4 {
                // Format 32 data comes back as one `long` per value, with one
                // x, y, width, height quadruple per desktop.
                let values = std::slice::from_raw_parts(data as *const c_long, items as usize);
                for quad in values.chunks_exact(4) {
                    if quad[2] > 0 && quad[3] > 0 {
                        area = Some((
                            quad[0] as f32,
                            quad[1] as f32,
                            quad[2] as f32,
                            quad[3] as f32,
                        ));
                        break;
                    }
                }
            }
            if !data.is_null() {
                free(data as *mut c_void);
            }
        }

        close(display);
        area
    }
}

/// Every non-Linux host: there is no X11 root window to read, so the caller
/// uses the size the compositor reports through the viewport instead.
#[cfg(not(target_os = "linux"))]
fn x11_work_area() -> Option<(f32, f32, f32, f32)> {
    None
}

fn default_out_dir() -> String {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("iPhoneBackups");
    p.display().to_string()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The app icon (assets/ibackup-256.png compiled in) shown in the window bar
/// and taskbar on all three operating systems.
pub(crate) fn window_icon() -> egui::IconData {
    let png = include_bytes!("../assets/ibackup-256.png");
    let img = image::load_from_memory(png)
        .expect("bad compiled-in icon")
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    egui::IconData {
        width: w,
        height: h,
        rgba: img.into_raw(),
    }
}

/// The same PNG as a texture, for the header mark.
pub(crate) fn load_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let png = include_bytes!("../assets/ibackup-256.png");
    let img = image::load_from_memory(png).ok()?.into_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture("ibackup-icon", color, egui::TextureOptions::LINEAR))
}

pub fn run() -> Result<()> {
    util::set_ui_mode(true);
    let (tx, rx) = channel::<Task>();
    let core = Arc::new(Mutex::new(UiCore {
        ctx: egui::Context::default(),
        tx: Some(tx.clone()),
        busy: None,
        devices: Vec::new(),
        devices_error: None,
        job: Job::default(),
    }));

    let worker_core = core.clone();
    let worker = std::thread::Builder::new()
        .name("ibackup-worker".into())
        .spawn(move || worker_loop(rx, worker_core));

    let native = eframe::NativeOptions {
        // eframe remembers the last window size and re-applies it right after the
        // window exists, which undoes whatever the window manager decided when it
        // honoured the "maximized" hint. The app always opens full screen, so the
        // remembered size is not worth having.
        persist_window: false,
        viewport: egui::ViewportBuilder::default()
            .with_min_inner_size([900.0, 600.0])
            .with_maximized(true)
            .with_title("ibackup")
            .with_icon(window_icon()),
        ..Default::default()
    };

    let r = eframe::run_native(
        "ibackup",
        native,
        Box::new(|cc| Ok(Box::new(App::new(cc, tx, core.clone())) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow::anyhow!("GUI: {e}"));
    // Back to plain CLI output for anything that runs after the window.
    util::set_ui_mode(false);
    // Drop the worker's own way to send itself work, so `recv` ends and the
    // join below cannot block on a thread that is merely waiting for input.
    if let Ok(mut c) = core.lock() {
        c.tx = None;
    }
    // Join a worker that is idle. If a copy is still in flight there is nothing
    // to wait for: the window is gone, and letting main return ends the process
    // the same way closing the window always did. Stages resume next time.
    if let Ok(h) = worker {
        let idle = core.lock().map(|c| c.busy.is_none()).unwrap_or(true);
        if idle {
            let _ = h.join();
        }
    }
    r
}
